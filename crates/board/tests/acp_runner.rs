//! Integration tests for headless [`board::acp_runner::AcpRunner`], Linux systemd/cgroup
//! command construction, process-group teardown, and [`board::runner::RunnerRegistry`].
//!
//! Uses a deterministic POSIX `/bin/sh` fake ACP stdio server (no bare `python3` or `node`)
//! speaking newline-delimited JSON-RPC 2.0 (`initialize`, `session/new`, `session/load`,
//! `session/prompt`, `session/update`, `session/cancel`).

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use board::acp_runner::{AcpRunner, build_launch_command};
use board::config::{AcpPermissionPolicy, AcpRunnerConfig, CgroupConfig, Home, ThreadModel};
use board::local_db::{LocalDb, LocalRunState};
use board::runner::{
    Clock, FakeRunner, RunEventKind, RunSpec, Runner, RunnerRegistry, RunnerState, SteerMessage,
};
use board_proto::Timestamp;

fn fixed_clock(ts: &str) -> Clock {
    let parsed: Timestamp = ts.parse().unwrap();
    Arc::new(move || parsed)
}

/// Deterministic POSIX `/bin/sh` fake ACP JSON-RPC 2.0 stdio server controlled via env vars:
/// - `FAKE_ACP_DIR`: scratch directory for spawn counts, request logs, session persistence, and
///   grandchild PID markers.
/// - `FAKE_ACP_EXIT_AFTER_TURN=1`: exits 0 right after emitting the `session/prompt` response.
/// - `FAKE_ACP_REJECT_LOAD=1`: responds with JSON-RPC `-32601` to `session/load`.
/// - `FAKE_ACP_TURN_DELAY`: sleep seconds before finishing `session/prompt` (e.g. `0.25`).
/// - `FAKE_ACP_IGNORE_CANCEL=1`: traps `SIGTERM`, spawns a `SIGTERM`-ignoring grandchild, logs
///   `cancel.seen` on `session/cancel`, and refuses to exit until `SIGKILL`.
/// - `FAKE_ACP_HANG_PROMPT=1`: spawns a grandchild and never answers `session/prompt`.
fn write_fake_acp_server(dir: &Path) -> PathBuf {
    let path = dir.join("fake-acp-agent");
    let script = r#"#!/bin/sh
set -u

STATE_DIR="${FAKE_ACP_DIR:?}"
mkdir -p "$STATE_DIR"

count_file="$STATE_DIR/spawns"
if [ -f "$count_file" ]; then
    spawns=$(cat "$count_file")
else
    spawns=0
fi
spawns=$((spawns + 1))
printf '%s\n' "$spawns" > "$count_file"

if [ -n "${ACP_TEST_SECRET:-}" ]; then
    printf '%s\n' "$ACP_TEST_SECRET" > "$STATE_DIR/secret_received"
fi

if [ "${FAKE_ACP_IGNORE_CANCEL:-0}" = "1" ]; then
    trap '' TERM
fi

extract_id() {
    printf '%s\n' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'
}

extract_session_id() {
    printf '%s\n' "$1" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p'
}

current_session=""
turn=0

while IFS= read -r line; do
    printf '%s\n' "$line" >> "$STATE_DIR/requests.jsonl"
    case "$line" in
        *'"method":"initialize"'*)
            id=$(extract_id "$line")
            if [ "${FAKE_ACP_HANDSHAKE_CLIENT_REQ:-0}" = "1" ]; then
                printf '{"jsonrpc":"2.0","id":999,"method":"client/ping","params":{}}\n'
                IFS= read -r client_resp
                printf '%s\n' "$client_resp" > "$STATE_DIR/handshake_client_resp.json"
            fi
            if [ "${FAKE_ACP_HANDSHAKE_PERM_REQ:-0}" = "1" ]; then
                perm_opts="${FAKE_ACP_HANDSHAKE_PERM_OPTIONS:-"[{\"optionId\":\"allow-once\",\"kind\":\"allow_once\"},{\"optionId\":\"allow-always\",\"kind\":\"allow_always\"},{\"optionId\":\"reject-once\",\"kind\":\"reject_once\"}]"}"
                printf '{"jsonrpc":"2.0","id":998,"method":"session/request_permission","params":{"toolCall":{"toolCallId":"hs-1","title":"Read workspace","kind":"read"},"options":%s}}\n' "$perm_opts"
                IFS= read -r perm_resp
                printf '%s\n' "$perm_resp" > "$STATE_DIR/handshake_perm_resp.json"
            fi
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}\n' "$id"
            ;;
        *'"method":"session/new"'*)
            id=$(extract_id "$line")
            n=1
            if [ -f "$STATE_DIR/session_seq" ]; then
                n=$(cat "$STATE_DIR/session_seq")
                n=$((n + 1))
            fi
            printf '%s\n' "$n" > "$STATE_DIR/session_seq"
            current_session="acp-sess-$n"
            printf '%s\n' "$current_session" >> "$STATE_DIR/sessions"
            printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$current_session"
            ;;
        *'"method":"session/load"'*)
            id=$(extract_id "$line")
            req_sess=$(extract_session_id "$line")
            if [ "${FAKE_ACP_REJECT_LOAD:-0}" = "1" ]; then
                printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"session/load unsupported"}}\n' "$id"
            elif grep -qx "$req_sess" "$STATE_DIR/sessions" 2>/dev/null; then
                current_session="$req_sess"
                printf '%s\n' "$req_sess" >> "$STATE_DIR/loaded_sessions"
                printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$current_session"
            else
                printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32001,"message":"unknown session"}}\n' "$id"
            fi
            ;;
        *'"method":"session/prompt"'*)
            id=$(extract_id "$line")
            turn=$((turn + 1))
            if [ "${FAKE_ACP_PROMPT_ERROR:-0}" = "1" ]; then
                if [ -n "${FAKE_ACP_TURN_DELAY:-}" ]; then
                    sleep "$FAKE_ACP_TURN_DELAY"
                fi
                printf '%s\n' "$$" > "$STATE_DIR/child.pid"
                printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"simulated prompt failure"}}\n' "$id"
            elif [ "${FAKE_ACP_IGNORE_CANCEL:-0}" = "1" ] || [ "${FAKE_ACP_HANG_PROMPT:-0}" = "1" ]; then
                (
                    trap '' TERM
                    sleep 30
                ) >/dev/null 2>&1 &
                printf '%s\n' "$!" > "$STATE_DIR/grandchild.pid"
                printf '%s\n' "$$" > "$STATE_DIR/child.pid"
                printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Working indefinitely"}}}}\n' "$current_session"
            else
                if [ -n "${FAKE_ACP_TURN_DELAY:-}" ]; then
                    sleep "$FAKE_ACP_TURN_DELAY"
                fi
                if [ "${FAKE_ACP_TURN_PERM_REQ:-0}" = "1" ]; then
                    perm_opts="${FAKE_ACP_TURN_PERM_OPTIONS:-"[{\"optionId\":\"allow-once\",\"kind\":\"allow_once\"},{\"optionId\":\"allow-always\",\"kind\":\"allow_always\"},{\"optionId\":\"reject-once\",\"kind\":\"reject_once\"}]"}"
                    printf '{"jsonrpc":"2.0","id":997,"method":"session/request_permission","params":{"sessionId":"%s","toolCall":{"toolCallId":"tc-perm-%s","title":"Run shell command","kind":"execute"},"options":%s}}\n' "$current_session" "$turn" "$perm_opts"
                    IFS= read -r perm_resp
                    printf '%s\n' "$perm_resp" > "$STATE_DIR/turn_perm_resp.json"
                fi
                printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Planning turn %s"}}}}\n' "$current_session" "$turn"
                printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"tool_call","toolCallId":"tc-%s","title":"cargo check","kind":"execute","status":"in_progress"}}}\n' "$current_session" "$turn"
                printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"tool_call_update","toolCallId":"tc-%s","title":"cargo check","status":"completed","text":"0 errors"}}}\n' "$current_session" "$turn"
                printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Completed turn %s"}}}}\n' "$current_session" "$turn"
                printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
                if [ "${FAKE_ACP_EXIT_AFTER_TURN:-0}" = "1" ]; then
                    exit 0
                fi
            fi
            ;;
        *'"method":"session/cancel"'*)
            printf 'cancelled\n' > "$STATE_DIR/cancel.seen"
            if [ "${FAKE_ACP_IGNORE_CANCEL:-0}" != "1" ]; then
                exit 0
            fi
            ;;
    esac
done
"#;
    common::script::write_script(&path, script);
    path
}

fn pid_alive(pid: u32) -> bool {
    let Some(raw) = i32::try_from(pid).ok().filter(|&p| p > 0) else {
        return false;
    };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(rustix::io::Errno::SRCH) => false,
        Err(_) => true,
    }
}

async fn wait_for<F, Fut>(timeout: Duration, mut pred: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if pred().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met within {timeout:?}");
}

#[tokio::test]
async fn spawn_streams_normalized_events_writes_transcript_and_persists_session_in_local_db() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path().join("home"));
    let workdir = dir.path().join("workdir");
    let fake_state = dir.path().join("fake-state");
    std::fs::create_dir_all(&workdir).unwrap();
    let bin = write_fake_acp_server(dir.path());
    let db = LocalDb::open_for_home(&home).unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_ACP_DIR".to_owned(),
        fake_state.to_string_lossy().into_owned(),
    );
    env.insert("ACP_TEST_SECRET".to_owned(), "do-not-log-secret".to_owned());

    let cfg = AcpRunnerConfig {
        bin: Some(bin),
        args: vec![],
        env,
        model: Some("default-model".to_owned()),
        options: [("verbosity".to_owned(), "low".to_owned())].into(),
        turn_timeout_secs: 30,
        cancel_grace_secs: 1,
        permission_policy: AcpPermissionPolicy::Allow,
        cgroup: CgroupConfig::default(),
    };
    let runner = AcpRunner::new(cfg, home.clone(), db.clone())
        .with_clock(fixed_clock("2026-11-24T10:00:00Z"));
    assert_eq!(runner.kind(), "acp");

    let spec = RunSpec {
        run_id: "run-stream-1".to_owned(),
        title: "agent-board-683 Test AcpRunner".to_owned(),
        prompt: "Inspect the repository and run checks".to_owned(),
        model: Some(ThreadModel {
            provider: "antigravity".to_owned(),
            model: "gemini-4-neon".to_owned(),
            options: [("thought_level".to_owned(), "high".to_owned())].into(),
        }),
        worktree: None,
        metadata: BTreeMap::from([
            ("board".to_owned(), "agent-board".to_owned()),
            ("task_id".to_owned(), "agent-board-683".to_owned()),
            ("kind".to_owned(), "worker".to_owned()),
        ]),
    };

    let handle = runner.spawn(&spec, &workdir).await.unwrap();
    assert_eq!(handle.runner, "acp");
    assert_eq!(handle.native_id, "acp-sess-1");
    assert_eq!(handle.workdir.as_deref(), Some(workdir.as_path()));
    assert_eq!(
        handle.agent_id.as_ref().map(|a| a.as_str()),
        Some("antigravity:acp-sess-1")
    );
    assert_eq!(
        handle.resolve_agent_id().unwrap().as_str(),
        "antigravity:acp-sess-1"
    );

    // LocalDb persisted session_id and live pid immediately on spawn.
    let db_row = db.get_local_run("run-stream-1").unwrap().unwrap();
    assert_eq!(db_row.runner, "acp");
    assert_eq!(db_row.session_id.as_deref(), Some("acp-sess-1"));
    assert!(db_row.pid.is_some());
    assert_eq!(db_row.board.as_str(), "agent-board");
    assert_eq!(
        db.get_local_run_by_session("acp-sess-1")
            .unwrap()
            .unwrap()
            .run_id,
        "run-stream-1"
    );

    // Wait for turn completion -> Idle.
    wait_for(Duration::from_secs(5), || async {
        runner.status(&handle).await.unwrap().state == RunnerState::Idle
    })
    .await;

    let status = runner.status(&handle).await.unwrap();
    assert_eq!(status.state, RunnerState::Idle);
    assert!(status.runnable);
    assert!(!status.busy);
    assert_eq!(status.runs, Some(1));
    assert_eq!(status.workdir.as_deref(), Some(workdir.as_path()));

    // Verify normalized RunEvents and monotonic seq pagination.
    let events = runner.poll_events(&handle, 0).await.unwrap();
    assert!(events.len() >= 6, "expected >= 6 events, got {events:#?}");
    for pair in events.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq + 1, "seq must be monotonic");
    }
    let kinds: Vec<RunEventKind> = events.iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&RunEventKind::Phase), "{kinds:?}");
    assert!(kinds.contains(&RunEventKind::Tool), "{kinds:?}");
    assert!(kinds.contains(&RunEventKind::Note), "{kinds:?}");
    assert!(kinds.contains(&RunEventKind::Done), "{kinds:?}");

    let tail = runner.poll_events(&handle, 3).await.unwrap();
    assert!(tail.iter().all(|e| e.seq > 3));
    assert_eq!(tail.len(), events.len() - 3);
    let empty = runner
        .poll_events(&handle, events.last().unwrap().seq)
        .await
        .unwrap();
    assert!(empty.is_empty());

    // Verify env was forwarded to child, model + merged options were sent in session/new, and
    // secret env value NEVER leaked into the local transcript.
    assert_eq!(
        std::fs::read_to_string(fake_state.join("secret_received"))
            .unwrap()
            .trim(),
        "do-not-log-secret"
    );
    let requests = std::fs::read_to_string(fake_state.join("requests.jsonl")).unwrap();
    assert!(requests.contains("\"method\":\"initialize\""), "{requests}");
    assert!(
        requests.contains("\"method\":\"session/new\""),
        "{requests}"
    );
    assert!(
        requests.contains("\"model\":\"gemini-4-neon\""),
        "{requests}"
    );
    assert!(
        requests.contains("\"thought_level\":\"high\""),
        "{requests}"
    );
    assert!(requests.contains("\"verbosity\":\"low\""), "{requests}");

    let transcript_path = home.state_dir().join("transcripts/run-stream-1.jsonl");
    assert!(
        transcript_path.exists(),
        "missing {}",
        transcript_path.display()
    );
    let transcript = std::fs::read_to_string(&transcript_path).unwrap();
    assert!(transcript.contains("\"session/new\""), "{transcript}");
    assert!(transcript.contains("\"session/update\""), "{transcript}");
    assert!(
        !transcript.contains("do-not-log-secret"),
        "env secret leaked into transcript!"
    );

    runner.cancel(&handle, "test cleanup").await.unwrap();
}

#[tokio::test]
async fn send_queues_on_live_process_and_resumes_idle_exited_session_via_session_load() {
    // Part A: Live process queueing while busy + follow-up while idle (single process spawn).
    {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let workdir = dir.path().join("workdir");
        let fake_state = dir.path().join("fake-state");
        std::fs::create_dir_all(&workdir).unwrap();
        let bin = write_fake_acp_server(dir.path());
        let db = LocalDb::open_for_home(&home).unwrap();

        let mut env = BTreeMap::new();
        env.insert(
            "FAKE_ACP_DIR".to_owned(),
            fake_state.to_string_lossy().into_owned(),
        );
        env.insert("FAKE_ACP_TURN_DELAY".to_owned(), "0.2".to_owned());

        let runner = AcpRunner::new(
            AcpRunnerConfig {
                bin: Some(bin),
                args: vec![],
                env,
                ..AcpRunnerConfig::default()
            },
            home,
            db,
        );
        let spec = RunSpec {
            run_id: "run-live-send".to_owned(),
            title: "Live send test".to_owned(),
            prompt: "Turn 1".to_owned(),
            model: None,
            worktree: None,
            metadata: BTreeMap::new(),
        };
        let handle = runner.spawn(&spec, &workdir).await.unwrap();

        // Send turn 2 while turn 1 is still sleeping/running -> queued cleanly.
        runner
            .send(
                &handle,
                &SteerMessage::new("Turn 2 queued").with_digest("abc123"),
            )
            .await
            .unwrap();

        wait_for(Duration::from_secs(5), || async {
            let st = runner.status(&handle).await.unwrap();
            st.state == RunnerState::Idle && st.runs == Some(2)
        })
        .await;

        // Send turn 3 while idle on the live process.
        runner
            .send(&handle, &SteerMessage::new("Turn 3 idle"))
            .await
            .unwrap();

        wait_for(Duration::from_secs(5), || async {
            let st = runner.status(&handle).await.unwrap();
            st.state == RunnerState::Idle && st.runs == Some(3)
        })
        .await;

        // All 3 turns ran on a single live process without respawning.
        assert_eq!(
            std::fs::read_to_string(fake_state.join("spawns"))
                .unwrap()
                .trim(),
            "1"
        );
        let events = runner.poll_events(&handle, 0).await.unwrap();
        let done_count = events
            .iter()
            .filter(|e| e.kind == RunEventKind::Done)
            .count();
        assert_eq!(done_count, 3, "expected 3 Done events, got {events:#?}");
        // Queued steering message was recorded as RunEventKind::Message with wake_digest for
        // daemon::read_messages verification.
        assert!(
            events.iter().any(|e| {
                e.kind == RunEventKind::Message
                    && e.metadata.get("wake_digest").map(String::as_str) == Some("abc123")
            }),
            "missing RunEventKind::Message with wake_digest: {events:#?}"
        );

        runner.cancel(&handle, "test cleanup").await.unwrap();
    }

    // Part B: Clean idle exit -> respawn + session/load resume (even from a fresh AcpRunner
    // instance recovering the persisted session_id from LocalDb).
    {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let workdir = dir.path().join("workdir");
        let fake_state = dir.path().join("fake-state");
        std::fs::create_dir_all(&workdir).unwrap();
        let bin = write_fake_acp_server(dir.path());
        let db = LocalDb::open_for_home(&home).unwrap();

        let mut env = BTreeMap::new();
        env.insert(
            "FAKE_ACP_DIR".to_owned(),
            fake_state.to_string_lossy().into_owned(),
        );
        env.insert("FAKE_ACP_EXIT_AFTER_TURN".to_owned(), "1".to_owned());

        let cfg = AcpRunnerConfig {
            bin: Some(bin),
            args: vec![],
            env,
            ..AcpRunnerConfig::default()
        };
        let runner1 = AcpRunner::new(cfg.clone(), home.clone(), db.clone());
        let spec = RunSpec {
            run_id: "run-resume-load".to_owned(),
            title: "Resume via session/load".to_owned(),
            prompt: "Turn 1 before idle exit".to_owned(),
            model: None,
            worktree: None,
            metadata: BTreeMap::new(),
        };
        let handle = runner1.spawn(&spec, &workdir).await.unwrap();
        assert_eq!(handle.native_id, "acp-sess-1");

        // Wait for clean exit -> LocalDb marks Finished, clears pid, preserves session_id.
        wait_for(Duration::from_secs(5), || async {
            db.get_local_run("run-resume-load")
                .unwrap()
                .is_some_and(|r| r.state == LocalRunState::Finished && r.pid.is_none())
        })
        .await;
        let row = db.get_local_run("run-resume-load").unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("acp-sess-1"));

        // Fresh AcpRunner instance (simulating daemon restart) resumes from LocalDb via session/load.
        let runner2 = AcpRunner::new(cfg, home.clone(), db.clone());
        let pre_status = runner2.status(&handle).await.unwrap();
        assert_eq!(pre_status.state, RunnerState::Idle);
        assert!(pre_status.runnable);

        runner2
            .send(&handle, &SteerMessage::new("Turn 2 via session/load"))
            .await
            .unwrap();

        wait_for(Duration::from_secs(5), || async {
            let events = runner2.poll_events(&handle, 0).await.unwrap();
            events.iter().any(|e| e.kind == RunEventKind::Done)
        })
        .await;

        assert_eq!(
            std::fs::read_to_string(fake_state.join("spawns"))
                .unwrap()
                .trim(),
            "2"
        );
        assert_eq!(
            std::fs::read_to_string(fake_state.join("loaded_sessions"))
                .unwrap()
                .trim(),
            "acp-sess-1"
        );
    }

    // Part C: session/load unsupported -> Trail Rule record + fallback to session/new + prompt delivery.
    {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let workdir = dir.path().join("workdir");
        let fake_state = dir.path().join("fake-state");
        std::fs::create_dir_all(&workdir).unwrap();
        let bin = write_fake_acp_server(dir.path());
        let db = LocalDb::open_for_home(&home).unwrap();

        let mut env = BTreeMap::new();
        env.insert(
            "FAKE_ACP_DIR".to_owned(),
            fake_state.to_string_lossy().into_owned(),
        );
        env.insert("FAKE_ACP_EXIT_AFTER_TURN".to_owned(), "1".to_owned());
        env.insert("FAKE_ACP_REJECT_LOAD".to_owned(), "1".to_owned());

        let runner = AcpRunner::new(
            AcpRunnerConfig {
                bin: Some(bin),
                args: vec![],
                env,
                ..AcpRunnerConfig::default()
            },
            home.clone(),
            db.clone(),
        )
        .with_clock(fixed_clock("2026-11-24T11:00:00Z"));

        let spec = RunSpec {
            run_id: "run-load-fallback".to_owned(),
            title: "Fallback when session/load unsupported".to_owned(),
            prompt: "Turn 1".to_owned(),
            model: None,
            worktree: None,
            metadata: BTreeMap::new(),
        };
        let handle = runner.spawn(&spec, &workdir).await.unwrap();
        wait_for(Duration::from_secs(5), || async {
            db.get_local_run("run-load-fallback")
                .unwrap()
                .is_some_and(|r| r.state == LocalRunState::Finished && r.pid.is_none())
        })
        .await;

        runner
            .send(&handle, &SteerMessage::new("Turn 2 after fallback"))
            .await
            .unwrap();

        wait_for(Duration::from_secs(5), || async {
            let events = runner.poll_events(&handle, 0).await.unwrap();
            events
                .iter()
                .filter(|e| e.kind == RunEventKind::Done)
                .count()
                >= 2
        })
        .await;

        // Trail Rule fallback entry written to state/acp-fallbacks.jsonl BEFORE session/new.
        let fallback_path = home.state_dir().join("acp-fallbacks.jsonl");
        assert!(fallback_path.exists());
        let line: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&fallback_path).unwrap().trim()).unwrap();
        assert_eq!(line["reason"], "session_load_unsupported");
        assert_eq!(line["previous_session_id"], "acp-sess-1");

        // LocalDb updated to replacement session_id and recorded alias so a fresh runner after
        // restart can still resolve the original handle.native_id ("acp-sess-1").
        let row = db.get_local_run("run-load-fallback").unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("acp-sess-2"));
        assert_eq!(
            db.get_local_run_by_session("acp-sess-1")
                .unwrap()
                .unwrap()
                .run_id,
            "run-load-fallback"
        );
        let events = runner.poll_events(&handle, 0).await.unwrap();
        assert!(events.iter().any(|e| {
            e.kind == RunEventKind::Note
                && e.metadata.get("fallback").map(String::as_str)
                    == Some("session_load_unsupported")
        }));
        wait_for(Duration::from_secs(5), || async {
            db.get_local_run("run-load-fallback")
                .unwrap()
                .is_some_and(|r| r.state == LocalRunState::Finished && r.pid.is_none())
        })
        .await;
        let fresh_runner = AcpRunner::new(runner.config().clone(), home, db);
        let recovered = fresh_runner.status(&handle).await.unwrap();
        assert_eq!(recovered.state, RunnerState::Idle);
        assert!(recovered.runnable);
    }
}

#[tokio::test]
async fn cancel_sends_session_cancel_writes_trail_and_kills_unresponsive_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path().join("home"));
    let workdir = dir.path().join("workdir");
    let fake_state = dir.path().join("fake-state");
    std::fs::create_dir_all(&workdir).unwrap();
    let bin = write_fake_acp_server(dir.path());
    let db = LocalDb::open_for_home(&home).unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_ACP_DIR".to_owned(),
        fake_state.to_string_lossy().into_owned(),
    );
    env.insert("FAKE_ACP_IGNORE_CANCEL".to_owned(), "1".to_owned());

    let runner = AcpRunner::new(
        AcpRunnerConfig {
            bin: Some(bin),
            args: vec![],
            env,
            ..AcpRunnerConfig::default()
        },
        home.clone(),
        db.clone(),
    )
    .with_clock(fixed_clock("2026-11-24T12:00:00Z"))
    .with_cancel_grace(Duration::from_millis(120));

    let spec = RunSpec {
        run_id: "run-cancel-pg".to_owned(),
        title: "Unresponsive process group".to_owned(),
        prompt: "Hang forever".to_owned(),
        model: None,
        worktree: None,
        metadata: BTreeMap::new(),
    };
    let handle = runner.spawn(&spec, &workdir).await.unwrap();

    // Wait until both child and its SIGTERM-ignoring grandchild are running.
    wait_for(Duration::from_secs(5), || async {
        fake_state.join("grandchild.pid").exists() && fake_state.join("child.pid").exists()
    })
    .await;
    let child_pid: u32 = std::fs::read_to_string(fake_state.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let grandchild_pid: u32 = std::fs::read_to_string(fake_state.join("grandchild.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(child_pid), "child {child_pid} should be alive");
    assert!(
        pid_alive(grandchild_pid),
        "grandchild {grandchild_pid} should be alive"
    );

    runner
        .cancel(&handle, "preempted by higher-priority task")
        .await
        .unwrap();

    // 1. ACP session/cancel was delivered before signals.
    assert!(
        fake_state.join("cancel.seen").exists(),
        "fake ACP server never saw session/cancel"
    );

    // 2. Trail Rule record was written to state/runner-cancels.jsonl.
    let trail_path = home.state_dir().join("runner-cancels.jsonl");
    assert!(trail_path.exists());
    let line: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&trail_path).unwrap().trim()).unwrap();
    assert_eq!(line["at"], "2026-11-24T12:00:00Z");
    assert_eq!(line["runner"], "acp");
    assert_eq!(line["native_id"], "acp-sess-1");
    assert_eq!(line["reason"], "preempted by higher-priority task");
    assert_eq!(line["supported"], true);
    assert_eq!(line["outcome"], "cancelled");

    // 3. Entire process group (both child AND SIGTERM-ignoring grandchild) is dead.
    assert!(!pid_alive(child_pid), "child {child_pid} leaked!");
    assert!(
        !pid_alive(grandchild_pid),
        "grandchild {grandchild_pid} leaked!"
    );

    // 4. Status + LocalDb updated and further sends rejected.
    let status = runner.status(&handle).await.unwrap();
    assert_eq!(status.state, RunnerState::Cancelled);
    assert!(!status.runnable);
    assert!(!status.busy);
    let row = db.get_local_run("run-cancel-pg").unwrap().unwrap();
    assert_eq!(row.state, LocalRunState::Cancelled);
    assert_eq!(row.pid, None);
    assert!(
        runner
            .send(&handle, &SteerMessage::new("too late"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn turn_timeout_writes_trail_record_and_tears_down_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path().join("home"));
    let workdir = dir.path().join("workdir");
    let fake_state = dir.path().join("fake-state");
    std::fs::create_dir_all(&workdir).unwrap();
    let bin = write_fake_acp_server(dir.path());
    let db = LocalDb::open_for_home(&home).unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_ACP_DIR".to_owned(),
        fake_state.to_string_lossy().into_owned(),
    );
    env.insert("FAKE_ACP_HANG_PROMPT".to_owned(), "1".to_owned());

    let runner = AcpRunner::new(
        AcpRunnerConfig {
            bin: Some(bin),
            args: vec![],
            env,
            ..AcpRunnerConfig::default()
        },
        home.clone(),
        db.clone(),
    )
    .with_clock(fixed_clock("2026-11-24T13:00:00Z"))
    .with_turn_timeout(Duration::from_millis(250))
    .with_cancel_grace(Duration::from_millis(100));

    let spec = RunSpec {
        run_id: "run-timeout-pg".to_owned(),
        title: "Timed out turn".to_owned(),
        prompt: "Never answer".to_owned(),
        model: None,
        worktree: None,
        metadata: BTreeMap::new(),
    };
    let handle = runner.spawn(&spec, &workdir).await.unwrap();

    wait_for(Duration::from_secs(5), || async {
        runner.status(&handle).await.unwrap().state == RunnerState::Failed
    })
    .await;

    let grandchild_pid: u32 = std::fs::read_to_string(fake_state.join("grandchild.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        !pid_alive(grandchild_pid),
        "grandchild {grandchild_pid} leaked after turn timeout!"
    );

    let trail_path = home.state_dir().join("runner-cancels.jsonl");
    let line: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&trail_path).unwrap().trim()).unwrap();
    assert_eq!(line["outcome"], "turn_timeout");
    assert_eq!(line["runner"], "acp");

    let row = db.get_local_run("run-timeout-pg").unwrap().unwrap();
    assert_eq!(row.state, LocalRunState::Failed);
    assert_eq!(row.pid, None);
}

#[test]
fn linux_systemd_run_cgroup_flags_are_built_accurately() {
    let bin = Path::new("/opt/vendor/antigravity");
    let args = vec!["acp".to_owned()];

    let full = CgroupConfig {
        enabled: true,
        memory_max: Some("4G".to_owned()),
        cpu_quota: Some("200%".to_owned()),
        tasks_max: Some(256),
        slice: Some("agent-board-runs.slice".to_owned()),
    };
    let cmd = build_launch_command(bin, &args, &full, true);
    assert_eq!(cmd.program, OsString::from("systemd-run"));
    assert_eq!(
        cmd.args,
        vec![
            OsString::from("--user"),
            OsString::from("--scope"),
            OsString::from("--quiet"),
            OsString::from("-p"),
            OsString::from("MemoryMax=4G"),
            OsString::from("-p"),
            OsString::from("CPUQuota=200%"),
            OsString::from("-p"),
            OsString::from("TasksMax=256"),
            OsString::from("-p"),
            OsString::from("Slice=agent-board-runs.slice"),
            OsString::from("--"),
            OsString::from("/opt/vendor/antigravity"),
            OsString::from("acp"),
        ]
    );

    // Partial cgroup settings emit only the configured properties.
    let partial = CgroupConfig {
        enabled: true,
        memory_max: Some("2G".to_owned()),
        cpu_quota: None,
        tasks_max: None,
        slice: None,
    };
    let partial_cmd = build_launch_command(bin, &args, &partial, true);
    assert_eq!(
        partial_cmd.args,
        vec![
            OsString::from("--user"),
            OsString::from("--scope"),
            OsString::from("--quiet"),
            OsString::from("-p"),
            OsString::from("MemoryMax=2G"),
            OsString::from("--"),
            OsString::from("/opt/vendor/antigravity"),
            OsString::from("acp"),
        ]
    );
}

#[tokio::test]
async fn runner_registry_selects_acp_vs_t3_dynamically() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path().join("home"));
    let workdir = dir.path().join("workdir");
    let fake_state = dir.path().join("fake-state");
    std::fs::create_dir_all(&workdir).unwrap();
    let bin = write_fake_acp_server(dir.path());
    let db = LocalDb::open_for_home(&home).unwrap();

    let acp_runner = AcpRunner::new(
        AcpRunnerConfig {
            bin: Some(bin),
            args: vec![],
            env: [(
                "FAKE_ACP_DIR".to_owned(),
                fake_state.to_string_lossy().into_owned(),
            )]
            .into(),
            ..AcpRunnerConfig::default()
        },
        home.clone(),
        db,
    );
    let t3_runner = FakeRunner::new("t3").with_home(home);

    let registry = RunnerRegistry::new()
        .with_runner(Arc::new(acp_runner))
        .with_runner(Arc::new(t3_runner))
        .with_default("acp")
        .unwrap();

    assert_eq!(registry.kinds(), vec!["acp", "t3"]);
    assert_eq!(registry.resolve(None).unwrap().kind(), "acp");
    assert_eq!(registry.resolve(Some("t3")).unwrap().kind(), "t3");

    let acp_spec = RunSpec {
        run_id: "reg-acp-1".to_owned(),
        title: "Dispatch to ACP".to_owned(),
        prompt: "Run via ACP".to_owned(),
        model: None,
        worktree: None,
        metadata: BTreeMap::new(),
    };
    let acp_handle = registry
        .resolve(Some("acp"))
        .unwrap()
        .spawn(&acp_spec, &workdir)
        .await
        .unwrap();
    assert_eq!(acp_handle.runner, "acp");
    assert_eq!(
        registry.resolve_for_handle(&acp_handle).unwrap().kind(),
        "acp"
    );

    let t3_spec = RunSpec {
        run_id: "reg-t3-1".to_owned(),
        title: "Dispatch to T3".to_owned(),
        prompt: "Run via T3".to_owned(),
        model: None,
        worktree: None,
        metadata: BTreeMap::new(),
    };
    let t3_handle = registry
        .resolve(Some("t3"))
        .unwrap()
        .spawn(&t3_spec, &workdir)
        .await
        .unwrap();
    assert_eq!(t3_handle.runner, "t3");
    assert_eq!(
        registry.resolve_for_handle(&t3_handle).unwrap().kind(),
        "t3"
    );

    registry
        .resolve_for_handle(&acp_handle)
        .unwrap()
        .cancel(&acp_handle, "registry cleanup")
        .await
        .unwrap();
}

#[tokio::test]
async fn rpc_prompt_error_terminates_child_records_dropped_prompts_and_blocks_send() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path().join("home"));
    let workdir = dir.path().join("workdir");
    let fake_state = dir.path().join("fake-state");
    std::fs::create_dir_all(&workdir).unwrap();
    let bin = write_fake_acp_server(dir.path());
    let db = LocalDb::open_for_home(&home).unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_ACP_DIR".to_owned(),
        fake_state.to_string_lossy().into_owned(),
    );
    env.insert("FAKE_ACP_PROMPT_ERROR".to_owned(), "1".to_owned());
    env.insert("FAKE_ACP_TURN_DELAY".to_owned(), "0.2".to_owned());

    let runner = AcpRunner::new(
        AcpRunnerConfig {
            bin: Some(bin),
            args: vec![],
            env,
            ..AcpRunnerConfig::default()
        },
        home.clone(),
        db.clone(),
    )
    .with_cancel_grace(Duration::from_millis(100));

    let spec = RunSpec {
        run_id: "run-rpc-err".to_owned(),
        title: "RPC error cleanup".to_owned(),
        prompt: "Trigger error".to_owned(),
        model: None,
        worktree: None,
        metadata: BTreeMap::new(),
    };
    let handle = runner.spawn(&spec, &workdir).await.unwrap();
    // Queue a prompt while the failing turn is in flight.
    runner
        .send(
            &handle,
            &SteerMessage::new("queued before error").with_digest("dropme123"),
        )
        .await
        .unwrap();

    wait_for(Duration::from_secs(5), || async {
        runner.status(&handle).await.unwrap().state == RunnerState::Failed
    })
    .await;

    // Child exited and LocalDb cleared pid with Failed state.
    let child_pid: u32 = std::fs::read_to_string(fake_state.join("child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_for(Duration::from_secs(3), || async { !pid_alive(child_pid) }).await;
    let row = db.get_local_run("run-rpc-err").unwrap().unwrap();
    assert_eq!(row.state, LocalRunState::Failed);
    assert_eq!(row.pid, None);

    // Trail Rule recorded the dropped queued prompt.
    let drop_path = home.state_dir().join("acp-dropped-prompts.jsonl");
    assert!(drop_path.exists());
    let line: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&drop_path).unwrap().trim()).unwrap();
    assert_eq!(line["reason"], "rpc_error");
    assert_eq!(line["digest"], "dropme123");
    assert_eq!(line["prompt"], "queued before error");

    // Failed run cannot be resurrected by send().
    assert!(
        runner
            .send(&handle, &SteerMessage::new("should fail"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dropping_runner_kills_live_process_group_and_handshake_declines_client_requests() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path().join("home"));
    let workdir = dir.path().join("workdir");
    let fake_state = dir.path().join("fake-state");
    std::fs::create_dir_all(&workdir).unwrap();
    let bin = write_fake_acp_server(dir.path());
    let db = LocalDb::open_for_home(&home).unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_ACP_DIR".to_owned(),
        fake_state.to_string_lossy().into_owned(),
    );
    env.insert("FAKE_ACP_HANDSHAKE_CLIENT_REQ".to_owned(), "1".to_owned());
    env.insert("FAKE_ACP_HANG_PROMPT".to_owned(), "1".to_owned());

    let runner = AcpRunner::new(
        AcpRunnerConfig {
            bin: Some(bin),
            args: vec![],
            env,
            ..AcpRunnerConfig::default()
        },
        home,
        db,
    )
    .with_handshake_timeout(Duration::from_secs(5));

    let spec = RunSpec {
        run_id: "run-drop-cleanup".to_owned(),
        title: "Drop cleans up process group".to_owned(),
        prompt: "Hang until drop".to_owned(),
        model: None,
        worktree: None,
        metadata: BTreeMap::new(),
    };
    let _handle = runner.spawn(&spec, &workdir).await.unwrap();

    // Handshake declined the agent's client/ping request with -32601 instead of hanging.
    let resp: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(fake_state.join("handshake_client_resp.json"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(resp["id"], 999);
    assert_eq!(resp["error"]["code"], -32601);

    wait_for(Duration::from_secs(5), || async {
        fake_state.join("grandchild.pid").exists()
    })
    .await;
    let grandchild_pid: u32 = std::fs::read_to_string(fake_state.join("grandchild.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(grandchild_pid));

    // Dropping the last AcpRunner must run AcpRunnerInner::drop (no Arc cycle!) and kill
    // the entire process group including the grandchild.
    drop(runner);
    wait_for(Duration::from_secs(3), || async {
        !pid_alive(grandchild_pid)
    })
    .await;
}

#[tokio::test]
async fn permission_requests_are_handled_in_handshake_and_turn_with_trail_records() {
    struct Case<'a> {
        name: &'a str,
        policy: AcpPermissionPolicy,
        handshake_opts: &'a str,
        turn_opts: &'a str,
        expect_hs_outcome: &'a str,
        expect_hs_opt: Option<&'a str>,
        expect_turn_outcome: &'a str,
        expect_turn_opt: Option<&'a str>,
    }

    let cases = [
        Case {
            name: "allow-prefers-always-and-falls-back-to-once",
            policy: AcpPermissionPolicy::Allow,
            handshake_opts: r#"[{"optionId":"hs-once","kind":"allow_once"},{"optionId":"hs-always","kind":"allow_always"},{"optionId":"hs-reject","kind":"reject_once"}]"#,
            turn_opts: r#"[{"optionId":"t-reject","kind":"reject_once"},{"optionId":"t-once","kind":"allow_once"}]"#,
            expect_hs_outcome: "selected",
            expect_hs_opt: Some("hs-always"),
            expect_turn_outcome: "selected",
            expect_turn_opt: Some("t-once"),
        },
        Case {
            name: "allow-missing-allow-cancels",
            policy: AcpPermissionPolicy::Allow,
            handshake_opts: r#"[{"optionId":"hs-reject","kind":"reject_once"}]"#,
            turn_opts: r#"[{"optionId":"t-reject-always","kind":"reject_always"}]"#,
            expect_hs_outcome: "cancelled",
            expect_hs_opt: None,
            expect_turn_outcome: "cancelled",
            expect_turn_opt: None,
        },
        Case {
            name: "deny-selects-reject-or-cancels",
            policy: AcpPermissionPolicy::Deny,
            handshake_opts: r#"[{"optionId":"hs-allow","kind":"allow_always"},{"optionId":"hs-reject-always","kind":"reject_always"},{"optionId":"hs-reject-once","kind":"reject_once"}]"#,
            turn_opts: r#"[{"optionId":"t-allow","kind":"allow_once"}]"#,
            expect_hs_outcome: "selected",
            expect_hs_opt: Some("hs-reject-once"),
            expect_turn_outcome: "cancelled",
            expect_turn_opt: None,
        },
    ];

    for case in cases {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let workdir = dir.path().join("workdir");
        let fake_state = dir.path().join("fake-state");
        std::fs::create_dir_all(&workdir).unwrap();
        let bin = write_fake_acp_server(dir.path());
        let db = LocalDb::open_for_home(&home).unwrap();

        let mut env = BTreeMap::new();
        env.insert(
            "FAKE_ACP_DIR".to_owned(),
            fake_state.to_string_lossy().into_owned(),
        );
        env.insert("FAKE_ACP_HANDSHAKE_PERM_REQ".to_owned(), "1".to_owned());
        env.insert(
            "FAKE_ACP_HANDSHAKE_PERM_OPTIONS".to_owned(),
            case.handshake_opts.to_owned(),
        );
        env.insert("FAKE_ACP_TURN_PERM_REQ".to_owned(), "1".to_owned());
        env.insert(
            "FAKE_ACP_TURN_PERM_OPTIONS".to_owned(),
            case.turn_opts.to_owned(),
        );

        let runner = AcpRunner::new(
            AcpRunnerConfig {
                bin: Some(bin),
                args: vec![],
                env,
                permission_policy: case.policy,
                ..AcpRunnerConfig::default()
            },
            home.clone(),
            db,
        )
        .with_clock(fixed_clock("2026-11-24T12:00:00Z"))
        .with_handshake_timeout(Duration::from_secs(5));

        let run_id = format!("run-perm-{}", case.name);
        let spec = RunSpec {
            run_id: run_id.clone(),
            title: format!("Permission test {}", case.name),
            prompt: "Run tests".to_owned(),
            model: None,
            worktree: None,
            metadata: BTreeMap::new(),
        };
        let handle = runner.spawn(&spec, &workdir).await.unwrap();

        wait_for(Duration::from_secs(5), || async {
            runner
                .status(&handle)
                .await
                .is_ok_and(|s| s.state == RunnerState::Idle)
        })
        .await;

        // Verify JSON-RPC replies delivered to the child in BOTH handshake and turn loops.
        let hs_resp: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(fake_state.join("handshake_perm_resp.json"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(hs_resp["id"], 998, "{}: {hs_resp}", case.name);
        assert!(hs_resp.get("error").is_none(), "{}: {hs_resp}", case.name);
        assert_eq!(
            hs_resp["result"]["outcome"]["outcome"], case.expect_hs_outcome,
            "{}: {hs_resp}",
            case.name
        );
        assert_eq!(
            hs_resp["result"]["outcome"]["optionId"].as_str(),
            case.expect_hs_opt,
            "{}: {hs_resp}",
            case.name
        );

        let turn_resp: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(fake_state.join("turn_perm_resp.json"))
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(turn_resp["id"], 997, "{}: {turn_resp}", case.name);
        assert!(
            turn_resp.get("error").is_none(),
            "{}: {turn_resp}",
            case.name
        );
        assert_eq!(
            turn_resp["result"]["outcome"]["outcome"], case.expect_turn_outcome,
            "{}: {turn_resp}",
            case.name
        );
        assert_eq!(
            turn_resp["result"]["outcome"]["optionId"].as_str(),
            case.expect_turn_opt,
            "{}: {turn_resp}",
            case.name
        );

        // Trail Rule check 1: raw JSONL transcript has both runner permission_decision entries.
        let transcript_path = home
            .state_dir()
            .join("transcripts")
            .join(format!("{run_id}.jsonl"));
        let transcript_lines: Vec<serde_json::Value> = std::fs::read_to_string(&transcript_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let decisions: Vec<&serde_json::Value> = transcript_lines
            .iter()
            .filter(|l| l["dir"] == "runner" && l["payload"]["event"] == "permission_decision")
            .collect();
        assert_eq!(
            decisions.len(),
            2,
            "{}: expected 2 permission_decision records in transcript: {transcript_lines:?}",
            case.name
        );
        assert_eq!(decisions[0]["payload"]["tool"], "Read workspace");
        assert_eq!(decisions[0]["payload"]["policy"], case.policy.as_str());
        assert_eq!(decisions[0]["payload"]["outcome"], case.expect_hs_outcome);
        assert_eq!(
            decisions[0]["payload"]["option_id"].as_str(),
            case.expect_hs_opt
        );
        assert_eq!(decisions[1]["payload"]["tool"], "Run shell command");
        assert_eq!(decisions[1]["payload"]["policy"], case.policy.as_str());
        assert_eq!(decisions[1]["payload"]["outcome"], case.expect_turn_outcome);
        assert_eq!(
            decisions[1]["payload"]["option_id"].as_str(),
            case.expect_turn_opt
        );

        // Trail Rule check 2: RunEvent stream carries both handshake and turn permission events.
        let events = runner.poll_events(&handle, 0).await.unwrap();
        let perm_events: Vec<_> = events
            .iter()
            .filter(|ev| {
                ev.metadata.get("client_method").map(String::as_str)
                    == Some("session/request_permission")
            })
            .collect();
        assert_eq!(
            perm_events.len(),
            2,
            "{}: expected 2 permission RunEvents: {events:?}",
            case.name
        );
        assert_eq!(
            perm_events[0].metadata.get("tool").map(String::as_str),
            Some("Read workspace")
        );
        assert_eq!(
            perm_events[0].metadata.get("policy").map(String::as_str),
            Some(case.policy.as_str())
        );
        assert_eq!(
            perm_events[0].metadata.get("outcome").map(String::as_str),
            Some(case.expect_hs_outcome)
        );
        assert_eq!(
            perm_events[0].metadata.get("option_id").map(String::as_str),
            case.expect_hs_opt
        );
        assert_eq!(
            perm_events[1].metadata.get("tool").map(String::as_str),
            Some("Run shell command")
        );
        assert_eq!(
            perm_events[1].metadata.get("outcome").map(String::as_str),
            Some(case.expect_turn_outcome)
        );
        assert_eq!(
            perm_events[1].metadata.get("option_id").map(String::as_str),
            case.expect_turn_opt
        );
    }
}

#[tokio::test]
async fn close_tears_down_idle_acp_child_process_and_evicts_session_slot() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::at(dir.path().join("home"));
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let fake_state = dir.path().join("fake-state");
    std::fs::create_dir_all(&fake_state).unwrap();
    let script = write_fake_acp_server(dir.path());
    let db = LocalDb::open_for_home(&home).unwrap();

    let mut env = BTreeMap::new();
    env.insert(
        "FAKE_ACP_DIR".to_owned(),
        fake_state.to_string_lossy().into_owned(),
    );

    let runner = AcpRunner::new(
        AcpRunnerConfig {
            bin: Some(script),
            args: vec![],
            env,
            ..AcpRunnerConfig::default()
        },
        home.clone(),
        db.clone(),
    )
    .with_cancel_grace(Duration::from_millis(100));

    let spec = RunSpec {
        run_id: "run-close-idle".to_owned(),
        title: "Single-shot ACP judge".to_owned(),
        prompt: "Do triage".to_owned(),
        model: None,
        worktree: None,
        metadata: BTreeMap::new(),
    };
    let handle = runner.spawn(&spec, &workdir).await.unwrap();

    // Wait for turn 1 to finish and transition to Idle while the child stays alive waiting on stdin.
    wait_for(Duration::from_secs(5), || async {
        let st = runner.status(&handle).await.unwrap();
        st.state == RunnerState::Idle && !st.busy && st.runs == Some(1)
    })
    .await;

    let child_pid = db
        .get_local_run("run-close-idle")
        .unwrap()
        .unwrap()
        .pid
        .expect("spawn must persist ACP child pid in local_runs");
    assert!(
        pid_alive(child_pid),
        "idle ACP child process should still be alive before Runner::close"
    );

    // Calling close() shuts down the idle ACP child process cleanly without writing a cancel trail.
    runner.close(&handle).await;
    wait_for(Duration::from_secs(3), || async { !pid_alive(child_pid) }).await;

    // No spurious cancel trail entry was written for normal completion teardown.
    assert!(
        !home.state_dir().join("runner-cancels.jsonl").exists(),
        "normal Runner::close must not write runner-cancels.jsonl"
    );
}
