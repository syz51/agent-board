//! The launch check (`docs/DESIGN.md`, "Daemon dispatch", Device limit): before an agent
//! starts a thread through T3, the `PreToolUse` hook counts the turns that run on the
//! device and refuses the launch when it would pass the device's limit.
//!
//! A refusal loses nothing. The whole call is kept in a file on the device before the
//! hook answers, and a note on the task the agent works on records the time, the cause,
//! the thread and that file. The agent reads both places in the refusal.
//!
//! The check fails open: with no limit, no T3 answer in time or a record it could not
//! write, the launch runs. A turn the user starts is no tool call and so never comes here.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use board_proto::{AddNote, AgentId, NoteKind, Runtime, TaskId, TaskStatus, Timestamp};
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

use super::{HookEvent, Reply, T3_CLI_ENV, direct_client, nested_session, spawn_token};
use crate::client::{Client, TaskQuery};
use crate::config::{Config, Home};
use crate::daemon::device;
use crate::identity::{T3_THREAD_ENV, resolve_agent};
use crate::state;
use crate::t3::T3Cli;

/// The T3 tools that start a thread, as their MCP server names them.
const LAUNCH_TOOLS: [&str; 3] = ["delegate_task", "t3_thread_launch", "create_threads"];

/// What the hook of a launch may take in all: [`COUNT_BUDGET`], [`NOTE_BUDGET`] and the
/// local steps. `board install` gives the hook more than this.
pub(super) const BUDGET: Duration = Duration::from_secs(8);

/// How long T3 has to list the running threads; later, the launch runs unchecked.
const COUNT_BUDGET: Duration = Duration::from_millis(4500);

/// How long the server has to take the note of a refusal; later, the file is the record.
const NOTE_BUDGET: Duration = Duration::from_secs(2);

/// Whether the tool call of `input` starts a T3 thread on this device. One that names an
/// environment may start it on another device, which this one cannot count for.
pub(super) fn is_launch(input: &Value) -> bool {
    let named = input["tool_name"]
        .as_str()
        .and_then(|tool| tool.strip_prefix("mcp__"))
        .and_then(|tool| tool.rsplit_once("__"))
        .is_some_and(|(_, name)| LAUNCH_TOOLS.contains(&name));
    named && !names_environment(&input["tool_input"])
}

fn names_environment(call: &Value) -> bool {
    let given = |v: &Value| v.as_str().is_some_and(|s| !s.is_empty());
    given(&call["environmentId"])
        || given(&call["target"]["environmentId"])
        || call["threads"]
            .as_array()
            .is_some_and(|threads| threads.iter().any(|t| given(&t["target"]["environmentId"])))
}

/// How many threads the call starts: each of `create_threads`' list, else one.
fn threads_asked(call: &Value) -> usize {
    call["threads"].as_array().map_or(1, |t| t.len().max(1))
}

/// The record of a refused launch, as the file keeps it.
#[derive(Debug, Serialize)]
struct Refused<'a> {
    at: Timestamp,
    cause: &'a str,
    device: &'a str,
    limit: usize,
    /// The threads with a turn in progress when the launch was refused.
    running: Vec<&'a str>,
    /// How many threads the call asked for.
    asked: usize,
    /// The T3 thread that asked, when the hook ran in one.
    thread: Option<&'a str>,
    session: Option<&'a str>,
    tool: &'a str,
    tool_use: Option<&'a str>,
    /// The call as the agent made it: the brief, the title and every option.
    input: &'a Value,
}

/// Decides the launch of `input`: lets it run, or refuses it with a record.
pub(super) async fn check(
    home: &Home,
    runtime: Runtime,
    explicit_agent: Option<&str>,
    input: &Value,
) -> Result<Reply> {
    let config = Config::load(home)?;
    let Ok((server, device_name)) = config.require_login() else {
        return Ok(Reply::Nothing);
    };
    let default = device::default_run_limit(device::host_memory());
    let limit = device::run_limit(&config, Some(default));
    let bin = std::env::var_os(T3_CLI_ENV)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| config.t3_bin.clone());
    let t3 = T3Cli::locate(bin.as_deref());
    let counted = tokio::time::timeout(COUNT_BUDGET, device::running_threads(&t3, &config)).await;
    let Ok(running) = counted else {
        tracing::debug!("launch check: T3 listed no threads within {COUNT_BUDGET:?}; not checked");
        return Ok(Reply::Nothing);
    };
    let asked = threads_asked(&input["tool_input"]);
    if running.len().saturating_add(asked) <= limit {
        return Ok(Reply::Nothing);
    }
    let at = Utc::now();
    let thread = std::env::var(T3_THREAD_ENV).ok();
    let thread = thread.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let tool = input["tool_name"].as_str().unwrap_or_default();
    let tool_use = input["tool_use_id"].as_str().filter(|s| !s.is_empty());
    let load = load(device_name.as_str(), limit, running.len(), asked);
    let record = Refused {
        at,
        cause: &load,
        device: device_name.as_str(),
        limit,
        running: running.iter().map(String::as_str).collect(),
        asked,
        thread,
        session: input["session_id"].as_str().filter(|s| !s.is_empty()),
        tool,
        tool_use,
        input: &input["tool_input"],
    };
    let name = format!(
        "{}-{}",
        at.format("%Y%m%dT%H%M%S%3fZ"),
        tool_use.unwrap_or("call")
    );
    // Before the refusal: a call that is not kept is not refused.
    let file = state::save_refused_launch(home, &name, &record)?;
    let text = note_text(&load, tool, title(&input["tool_input"]), thread, &file);
    let noted = tokio::time::timeout(
        NOTE_BUDGET,
        note(home, server, runtime, explicit_agent, input, text),
    )
    .await;
    let noted = match noted {
        Ok(Ok(noted)) => noted,
        Ok(Err(err)) => {
            tracing::debug!("launch check: no note of the refusal: {err:#}");
            None
        }
        Err(_) => {
            tracing::debug!("launch check: no note of the refusal within {NOTE_BUDGET:?}");
            None
        }
    };
    Ok(Reply::Deny(refusal(&load, &file, noted.as_ref())))
}

/// The cause of a refusal, in one sentence.
fn load(device: &str, limit: usize, running: usize, asked: usize) -> String {
    let asks = if asked == 1 {
        "this launch asks for 1 more".to_owned()
    } else {
        format!("this launch asks for {asked} more")
    };
    format!("device {device} has {running} of {limit} agent turns running and {asks}")
}

/// The title the call gives its thread, or the titles of `create_threads`' list.
fn title(call: &Value) -> String {
    match call["threads"].as_array() {
        Some(threads) => threads
            .iter()
            .filter_map(|t| t["title"].as_str())
            .collect::<Vec<_>>()
            .join("; "),
        None => call["title"].as_str().unwrap_or_default().to_owned(),
    }
}

/// How much of a launch's title the note of its refusal quotes; the file has all of it.
const TITLE_CHARS: usize = 120;

fn note_text(load: &str, tool: &str, title: String, thread: Option<&str>, file: &Path) -> String {
    let title = if title.chars().count() > TITLE_CHARS {
        let cut: String = title.chars().take(TITLE_CHARS).collect();
        format!("{cut}…")
    } else {
        title
    };
    let thread = thread.map_or_else(String::new, |t| format!(" from thread {t}"));
    format!(
        "The device limit refused a launch{thread}: {load}. Tool {tool}, title \"{title}\". \
         The whole call is kept on the device in {}.",
        file.display()
    )
}

fn refusal(load: &str, file: &Path, noted: Option<&(TaskId, i64)>) -> String {
    let noted = noted.map_or_else(String::new, |(task, note)| {
        format!(" Note #{note} on {task} records the refusal.")
    });
    format!(
        "The device limit refused this launch: {load}. Nothing is lost: the whole call is \
         kept in {}.{noted} Do the work in this thread, or make the same call again when a \
         turn on the device has ended. Do not start the thread another way.",
        file.display()
    )
}

/// Records the refusal as a note on the task the agent works on, when it has one, and
/// gives the task and the note's id.
async fn note(
    home: &Home,
    server: &str,
    runtime: Runtime,
    explicit_agent: Option<&str>,
    input: &Value,
    text: String,
) -> Result<Option<(TaskId, i64)>> {
    let session = input["session_id"].as_str().filter(|s| !s.is_empty());
    let nested = nested_session(
        home,
        HookEvent::PreToolUse,
        runtime,
        session,
        explicit_agent,
    )
    .await?;
    let env = |name: &str| std::env::var(name).ok();
    let Some(agent) = resolve_agent(explicit_agent, env, session.map(|s| (runtime, s)), nested)?
    else {
        return Ok(None);
    };
    let client = client(home, server, &agent).await?;
    let Some(client) = client else {
        return Ok(None);
    };
    let doing = client
        .tasks(&TaskQuery {
            owner: Some(agent.clone()),
            status: Some(TaskStatus::Doing),
            ..TaskQuery::default()
        })
        .await?;
    let Some(task) = doing.first() else {
        return Ok(None);
    };
    let body = AddNote {
        agent,
        kind: NoteKind::Note,
        text,
    };
    let note = client.add_note(&task.id, &body).await?;
    Ok(Some((task.id.clone(), note.id)))
}

/// The hook's client for `agent`: through the device daemon's relay, else with the device
/// token; none when the device has no token.
async fn client(home: &Home, server: &str, agent: &AgentId) -> Result<Option<Client>> {
    let socket = std::env::var_os("BOARD_TOKEN")
        .is_none_or(|t| t.is_empty())
        .then(|| crate::relay::safe_socket(home))
        .flatten();
    if let Some(socket) = &socket
        && let Some(client) = crate::relay::connect(socket, server).await
    {
        return Ok(Some(client.with_agent(Some(agent.clone()))));
    }
    let Some(token) = spawn_token(home).await?? else {
        return Ok(None);
    };
    let connects = std::sync::Arc::default();
    let client = direct_client(server, token, Some(agent.clone()), &connects)?;
    Ok(Some(client.with_timeout(NOTE_BUDGET)))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_tools_that_start_a_thread_are_launches() {
        let call = |tool: &str, input: Value| json!({"tool_name": tool, "tool_input": input});
        for tool in ["delegate_task", "t3_thread_launch", "create_threads"] {
            let tool = format!("mcp__t3-code__{tool}");
            assert!(is_launch(&call(&tool, json!({"title": "x"}))), "{tool}");
        }
        // Any name of the T3 server counts; other tools and other servers' tools do not.
        assert!(is_launch(&call("mcp__t3__delegate_task", json!({}))));
        assert!(!is_launch(&call("mcp__t3-code__t3_thread_send", json!({}))));
        assert!(!is_launch(&call("delegate_task", json!({}))));
        assert!(!is_launch(&call("Agent", json!({}))));
        assert!(!is_launch(&json!({})));
    }

    #[test]
    fn a_launch_that_names_an_environment_is_not_counted_here() {
        let call =
            |input: Value| json!({"tool_name": "mcp__t3-code__delegate_task", "tool_input": input});
        assert!(!is_launch(&call(
            json!({"target": {"environmentId": "env-2"}})
        )));
        assert!(!is_launch(&call(json!({"environmentId": "env-2"}))));
        assert!(!is_launch(&call(
            json!({"threads": [{"title": "a"}, {"target": {"environmentId": "env-2"}}]})
        )));
        assert!(is_launch(&call(json!({"target": {"model": "m"}}))));
        assert!(is_launch(&call(json!({"environmentId": ""}))));
    }

    #[test]
    fn create_threads_asks_for_each_thread_of_its_list() {
        assert_eq!(threads_asked(&json!({"task": "x"})), 1);
        assert_eq!(threads_asked(&json!({"threads": [{}, {}, {}]})), 3);
        assert_eq!(threads_asked(&json!({"threads": []})), 1);
    }

    #[test]
    fn the_note_of_a_refusal_fits_a_note_and_names_the_file() {
        let file = Path::new("/home/me/.local/share/agent-board/state/refused-launches/x.json");
        let cause = load("air", 4, 4, 1);
        assert_eq!(
            cause,
            "device air has 4 of 4 agent turns running and this launch asks for 1 more"
        );
        let text = note_text(
            &cause,
            "mcp__t3-code__delegate_task",
            "t".repeat(500),
            Some("thread-1"),
            file,
        );
        assert!(text.chars().count() <= 800, "{}", text.chars().count());
        assert!(text.contains("from thread thread-1"), "{text}");
        assert!(text.contains(&format!("\"{}…\"", "t".repeat(TITLE_CHARS))));
        assert!(text.ends_with(&format!("in {}.", file.display())), "{text}");
        let told = refusal(&cause, file, Some(&("factory-4".parse().unwrap(), 12)));
        assert!(told.contains("Note #12 on factory-4 records the refusal."));
        assert!(told.contains(&file.display().to_string()));
        assert_eq!(
            title(&json!({"threads": [{"title": "a"}, {"title": "b"}]})),
            "a; b"
        );
    }
}
