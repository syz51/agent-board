//! The launch check (`docs/DESIGN.md`, "Daemon dispatch", Device limit): before an agent
//! starts a thread through T3, the `PreToolUse` hook counts the turns that run on the
//! device and refuses the launch when it would pass the device's limit.
//!
//! A refusal loses nothing. The whole call is kept in a file on the device before the
//! hook answers, and a note on the task the agent works on records the time, the cause,
//! the thread and that file. The agent reads both places in the refusal. The same call,
//! refused again, adds its time and cause to the same file and no second note.
//!
//! A thread the call starts in another T3 environment runs on another device and is not
//! counted here; this device's environment is known from the session thread's T3 link.
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

use super::{HookEvent, Reply, direct_client, nested_session, spawn_token, t3_cli};
use crate::client::{Client, TaskQuery};
use crate::config::{Config, Home};
use crate::daemon::device;
use crate::identity::{T3_THREAD_ENV, resolve_agent};
use crate::state;

/// The T3 tools that start a thread, as their MCP server names them.
const LAUNCH_TOOLS: [&str; 3] = ["delegate_task", "t3_thread_launch", "create_threads"];

/// What the hook of a launch may take in all: [`COUNT_BUDGET`], [`NOTE_BUDGET`] and the
/// local steps. `board install` gives the hook more than this.
pub(super) const BUDGET: Duration = Duration::from_secs(8);

/// How long T3 has to list the running threads; later, the launch runs unchecked.
const COUNT_BUDGET: Duration = Duration::from_millis(4500);

/// How long the server has to take the note of a refusal; later, the file is the record.
const NOTE_BUDGET: Duration = Duration::from_secs(2);

/// Whether the tool call of `input` starts a T3 thread.
pub(super) fn is_launch(input: &Value) -> bool {
    input["tool_name"]
        .as_str()
        .and_then(|tool| tool.strip_prefix("mcp__"))
        .and_then(|tool| tool.rsplit_once("__"))
        .is_some_and(|(_, name)| LAUNCH_TOOLS.contains(&name))
}

/// The T3 environment a call or one thread of its list names, when it names one.
fn environment(of: &Value) -> Option<&str> {
    [&of["environmentId"], &of["target"]["environmentId"]]
        .into_iter()
        .find_map(|v| v.as_str().filter(|s| !s.is_empty()))
}

/// How many threads the call starts on this device: each of `create_threads`' list, else
/// one. A thread that names an environment starts there: it counts when that environment is
/// `local`, this device's. With `local` unknown it is not counted, since it may start on
/// another device, which this one cannot count for.
fn threads_asked(call: &Value, local: Option<&str>) -> usize {
    let here = |named: Option<&str>| named.is_none_or(|env| local == Some(env));
    match call["threads"].as_array() {
        Some(threads) => threads
            .iter()
            .filter(|t| here(environment(t).or_else(|| environment(call))))
            .count(),
        None => usize::from(here(environment(call))),
    }
}

/// The T3 environment of this device, read from the link T3 gave the session's thread
/// (`…/threads/<environment>/<thread>`), which the session's hooks cache.
fn local_environment(home: &Home, runtime: Runtime, session: Option<&str>) -> Option<String> {
    let link = state::load_session(home, runtime, session?)?.t3_link?;
    let (_, rest) = link.split_once("/threads/")?;
    let (environment, _) = rest.split_once('/')?;
    (!environment.is_empty()).then(|| environment.to_owned())
}

/// The name of the record of a refused call: the same call of the same session has the
/// same name, so that its repeats add to one record.
fn record_name(session: Option<&str>, tool: &str, call: &Value) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    for part in [session.unwrap_or_default(), tool, &call.to_string()] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    let digest = hash.finalize();
    digest[..12].iter().map(|b| format!("{b:02x}")).collect()
}

/// One more refusal of a call whose record is kept already.
#[derive(Debug, Serialize)]
struct Again<'a> {
    at: Timestamp,
    cause: &'a str,
    running: &'a std::collections::BTreeSet<String>,
    tool_use: Option<&'a str>,
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
    let session = input["session_id"].as_str().filter(|s| !s.is_empty());
    let local = local_environment(home, runtime, session);
    let asked = threads_asked(&input["tool_input"], local.as_deref());
    if asked == 0 {
        return Ok(Reply::Nothing);
    }
    let t3 = t3_cli(home);
    let counted = tokio::time::timeout(COUNT_BUDGET, device::running_threads(&t3, &config)).await;
    let Ok(running) = counted else {
        tracing::debug!("launch check: T3 listed no threads within {COUNT_BUDGET:?}; not checked");
        return Ok(Reply::Nothing);
    };
    if running.len().saturating_add(asked) <= limit {
        return Ok(Reply::Nothing);
    }
    let at = Utc::now();
    let thread = std::env::var(T3_THREAD_ENV).ok();
    let thread = thread.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let tool = input["tool_name"].as_str().unwrap_or_default();
    let tool_use = input["tool_use_id"].as_str().filter(|s| !s.is_empty());
    let load = load(device_name.as_str(), limit, running.len(), asked);
    let advice = advice(asked, limit);
    let name = record_name(session, tool, &input["tool_input"]);
    // The same call, refused before: its record takes this refusal too, and the task has
    // its note already.
    let again = Again {
        at,
        cause: &load,
        running: &running,
        tool_use,
    };
    if let Some(file) = state::repeat_refused_launch(home, &name, &again)? {
        return Ok(Reply::Deny(format!(
            "The device limit refused this launch again: {load}. Nothing is lost: the whole \
             call is kept in {}, and this refusal is added to it. {advice} Do not start the \
             thread another way.",
            file.display()
        )));
    }
    let record = Refused {
        at,
        cause: &load,
        device: device_name.as_str(),
        limit,
        running: running.iter().map(String::as_str).collect(),
        asked,
        thread,
        session,
        tool,
        tool_use,
        input: &input["tool_input"],
    };
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
    Ok(Reply::Deny(refusal(&load, &file, noted.as_ref(), &advice)))
}

/// What the agent of a refused launch does next. The device runs `limit` turns at most and
/// the agent's own turn is one of them, so a call for `limit` threads or more never fits.
fn advice(asked: usize, limit: usize) -> String {
    if asked >= limit {
        let fewer = if asked > 1 {
            "Ask for fewer threads, or do the work in this thread."
        } else {
            "Do the work in this thread."
        };
        format!(
            "This call can never run as it is: it asks for {asked} and the device runs \
             {limit} at most, your own turn among them. {fewer}"
        )
    } else if asked > 1 {
        "Do the work in this thread, ask for fewer threads, or make the same call again \
         when turns on the device have ended."
            .to_owned()
    } else {
        "Do the work in this thread, or make the same call again when a turn on the device \
         has ended."
            .to_owned()
    }
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

fn refusal(load: &str, file: &Path, noted: Option<&(TaskId, i64)>, advice: &str) -> String {
    let noted = noted.map_or_else(String::new, |(task, note)| {
        format!(" Note #{note} on {task} records the refusal.")
    });
    format!(
        "The device limit refused this launch: {load}. Nothing is lost: the whole call is \
         kept in {}.{noted} {advice} Do not start the thread another way.",
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
    fn a_thread_for_another_environment_is_not_counted_here() {
        let here = Some("env-1");
        assert_eq!(threads_asked(&json!({"task": "x"}), here), 1);
        assert_eq!(threads_asked(&json!({"task": "x"}), None), 1);
        assert_eq!(threads_asked(&json!({"environmentId": ""}), None), 1);
        // This device's environment, named, is this device.
        for call in [
            json!({"environmentId": "env-1"}),
            json!({"target": {"environmentId": "env-1", "model": "m"}}),
        ] {
            assert_eq!(threads_asked(&call, here), 1, "{call}");
            // Unknown here: it may be another device's.
            assert_eq!(threads_asked(&call, None), 0, "{call}");
            assert_eq!(threads_asked(&call, Some("env-2")), 0, "{call}");
        }
    }

    #[test]
    fn create_threads_asks_for_each_local_thread_of_its_list() {
        let here = Some("env-1");
        assert_eq!(threads_asked(&json!({"threads": [{}, {}, {}]}), here), 3);
        assert_eq!(threads_asked(&json!({"threads": []}), here), 0);
        // One thread for another device does not take the others out of the count.
        let mixed = json!({"threads": [
            {"title": "a"},
            {"target": {"environmentId": "env-2"}},
            {"target": {"environmentId": "env-1"}},
        ]});
        assert_eq!(threads_asked(&mixed, here), 2);
        assert_eq!(threads_asked(&mixed, None), 1);
        // The call's environment holds for the threads that name none.
        let all = json!({"environmentId": "env-2", "threads": [{}, {"environmentId": "env-1"}]});
        assert_eq!(threads_asked(&all, here), 1);
    }

    #[test]
    fn the_same_call_of_a_session_has_one_record_name() {
        let call = json!({"title": "a", "task": "b"});
        let name = record_name(Some("s1"), "mcp__t3-code__delegate_task", &call);
        assert_eq!(name.len(), 24);
        assert_eq!(
            name,
            record_name(Some("s1"), "mcp__t3-code__delegate_task", &call)
        );
        assert_ne!(
            name,
            record_name(Some("s2"), "mcp__t3-code__delegate_task", &call)
        );
        assert_ne!(
            name,
            record_name(Some("s1"), "mcp__t3-code__t3_thread_launch", &call)
        );
        let other = json!({"title": "a", "task": "c"});
        assert_ne!(
            name,
            record_name(Some("s1"), "mcp__t3-code__delegate_task", &other)
        );
    }

    #[test]
    fn the_advice_of_a_refusal_fits_what_was_asked() {
        assert!(advice(1, 4).contains("make the same call again"));
        assert!(advice(2, 4).contains("ask for fewer threads"));
        let never = advice(4, 4);
        assert!(never.contains("can never run as it is"), "{never}");
        assert!(never.contains("Ask for fewer threads"), "{never}");
        assert!(!never.contains("again"), "{never}");
        let one = advice(1, 1);
        assert!(one.contains("can never run"), "{one}");
        assert!(!one.contains("fewer"), "{one}");
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
        let told = refusal(
            &cause,
            file,
            Some(&("factory-4".parse().unwrap(), 12)),
            &advice(1, 4),
        );
        assert!(told.contains("Note #12 on factory-4 records the refusal."));
        assert!(told.contains(&file.display().to_string()));
        assert_eq!(
            title(&json!({"threads": [{"title": "a"}, {"title": "b"}]})),
            "a; b"
        );
    }
}
