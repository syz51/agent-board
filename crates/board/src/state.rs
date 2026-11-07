//! Small JSON files in `$BOARD_HOME/state` and `$BOARD_HOME/cache`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use board_proto::{AgentId, Runtime, Snapshot, TaskId, Timestamp};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::{Home, write_atomic};

/// What the hooks remember about one agent between invocations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentLocal {
    /// Event sequence of the last snapshot or delta injected.
    pub seq: Option<i64>,
    /// Last heartbeat and alert check from `PostToolUse`.
    pub last_heartbeat: Option<Timestamp>,
    /// When the current turn started (`UserPromptSubmit`).
    pub turn_started: Option<Timestamp>,
    /// What the last `Stop` handoff reminder was for: the change it reported (or the turn,
    /// against a server that does not date changes), so each is reminded about once.
    pub reminded_for: Option<Timestamp>,
    /// Set while the server is unreachable and the cached snapshot was already injected.
    pub offline_notified: bool,
    /// Tasks the `Stop` hook already reminded the agent to file what remains of, newest
    /// last, so each is reminded about once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done_reminded: Vec<TaskId>,
}

/// A file-system-safe name for an agent id.
pub fn file_stem(agent: &AgentId) -> String {
    safe_stem(agent.as_str())
}

fn safe_stem(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' {
                (b as char).to_string()
            } else {
                format!("_{b:02x}")
            }
        })
        .collect()
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(path, &serde_json::to_vec(value)?)
}

fn agent_state_path(home: &Home, agent: &AgentId) -> PathBuf {
    home.state_dir().join(format!("{}.json", file_stem(agent)))
}

fn cache_path(home: &Home, agent: &AgentId) -> PathBuf {
    home.cache_dir().join(format!("{}.json", file_stem(agent)))
}

/// The agent's hook state; missing or unreadable state starts fresh.
pub fn load_agent(home: &Home, agent: &AgentId) -> AgentLocal {
    read_json(&agent_state_path(home, agent)).unwrap_or_default()
}

pub fn save_agent(home: &Home, agent: &AgentId, state: &AgentLocal) -> Result<()> {
    write_json(&agent_state_path(home, agent), state)
}

/// The last full snapshot fetched for the agent.
pub fn load_cached_snapshot(home: &Home, agent: &AgentId) -> Option<Snapshot> {
    read_json(&cache_path(home, agent))
}

pub fn save_cached_snapshot(home: &Home, agent: &AgentId, snapshot: &Snapshot) -> Result<()> {
    write_json(&cache_path(home, agent), snapshot)
}

/// What the hooks decided once per runtime session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLocal {
    /// The session runs inside another agent session, so it does not take the T3 thread
    /// id it inherited (`identity::is_nested`).
    pub nested: bool,
    /// The T3 thread that delegated this session's thread, once T3 named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3_parent: Option<String>,
}

fn runtime_name(runtime: Runtime) -> &'static str {
    match runtime {
        Runtime::Claude => "claude",
        Runtime::Codex => "codex",
    }
}

fn session_path(home: &Home, runtime: Runtime, session: &str) -> PathBuf {
    home.state_dir().join("sessions").join(format!(
        "{}-{}.json",
        runtime_name(runtime),
        safe_stem(session)
    ))
}

/// The session's cached decisions; `None` when none were saved.
pub fn load_session(home: &Home, runtime: Runtime, session: &str) -> Option<SessionLocal> {
    read_json(&session_path(home, runtime, session))
}

pub fn save_session(
    home: &Home,
    runtime: Runtime,
    session: &str,
    state: &SessionLocal,
) -> Result<()> {
    write_json(&session_path(home, runtime, session), state)
}

/// Forgets an ended session; a missing file is fine.
pub fn remove_session(home: &Home, runtime: Runtime, session: &str) -> Result<()> {
    remove_if_exists(&session_path(home, runtime, session))
}

/// Removes a file; a missing one is fine.
fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
        _ => Ok(()),
    }
}

/// Removes the cached decisions of sessions not written for `age`: sessions that ended
/// without a SessionEnd hook. A session that comes back decides again.
pub fn prune_sessions(home: &Home, age: std::time::Duration) -> Result<()> {
    prune_dir(&home.state_dir().join("sessions"), age)
}

fn tool_start_path(home: &Home, runtime: Runtime, tool_use: &str) -> PathBuf {
    home.state_dir().join("tool-starts").join(format!(
        "{}-{}.json",
        runtime_name(runtime),
        safe_stem(tool_use)
    ))
}

/// When a shell tool call started, from its `PreToolUse` hook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStart {
    pub at: Timestamp,
    /// The git directory of the call's work tree then, so `head` is compared with the same
    /// work tree after the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dir: Option<PathBuf>,
    /// The commit checked out in the call's work tree then; `None` outside a repo, before
    /// the first commit, or when `git` did not answer in time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// Records when the tool call `tool_use` started. One file per call, so parallel calls
/// never write the same file.
pub fn save_tool_start(
    home: &Home,
    runtime: Runtime,
    tool_use: &str,
    start: &ToolStart,
) -> Result<()> {
    write_json(&tool_start_path(home, runtime, tool_use), start)
}

/// When the tool call `tool_use` started, forgetting it; `None` when no `PreToolUse` hook
/// recorded it.
pub fn take_tool_start(home: &Home, runtime: Runtime, tool_use: &str) -> Result<Option<ToolStart>> {
    let path = tool_start_path(home, runtime, tool_use);
    let start = read_json::<ToolStart>(&path);
    remove_if_exists(&path)?;
    Ok(start)
}

/// Removes start marks of tool calls that never reached `PostToolUse` (failed or
/// interrupted) after `age`.
pub fn prune_tool_starts(home: &Home, age: std::time::Duration) -> Result<()> {
    prune_dir(&home.state_dir().join("tool-starts"), age)
}

/// Removes the files in `dir` not written for `age`; a missing `dir` is fine.
fn prune_dir(dir: &Path, age: std::time::Duration) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    let now = std::time::SystemTime::now();
    for entry in entries {
        let entry = entry?;
        // Removed by another hook since the listing.
        let modified = match entry.metadata() {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            metadata => metadata?.modified()?,
        };
        if now
            .duration_since(modified)
            .is_ok_and(|elapsed| elapsed > age)
        {
            remove_if_exists(&entry.path())?;
        }
    }
    Ok(())
}

/// A task the agent marked done through `board done` or the `task_done` tool, kept until
/// the agent's next `Stop` hook reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoneMark {
    pub task: TaskId,
    pub at: Timestamp,
}

fn done_dir(home: &Home, agent: &AgentId) -> PathBuf {
    home.state_dir().join("done").join(file_stem(agent))
}

/// Records that `agent` marked `task` done. One file per task, so parallel calls never
/// write the same file.
pub fn record_done(home: &Home, agent: &AgentId, task: &TaskId, at: Timestamp) -> Result<()> {
    let mark = DoneMark {
        task: task.clone(),
        at,
    };
    write_json(
        &done_dir(home, agent).join(format!("{}.json", safe_stem(task.as_str()))),
        &mark,
    )
}

/// The tasks `agent` marked done since the last call, oldest first, forgetting them.
pub fn take_done(home: &Home, agent: &AgentId) -> Result<Vec<DoneMark>> {
    let entries = match std::fs::read_dir(done_dir(home, agent)) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    let mut marks = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if let Some(mark) = read_json::<DoneMark>(&path) {
            marks.push(mark);
        }
        remove_if_exists(&path)?;
    }
    marks.sort_by_key(|m| m.at);
    Ok(marks)
}

/// Removes done marks no `Stop` hook read for `age` (a session that ended mid-turn) and
/// the agents' directories they leave empty.
pub fn prune_done(home: &Home, age: std::time::Duration) -> Result<()> {
    let entries = match std::fs::read_dir(home.state_dir().join("done")) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        entries => entries?,
    };
    for entry in entries {
        let dir = entry?.path();
        prune_dir(&dir, age)?;
        match std::fs::remove_dir(&dir) {
            // Still holds marks, or another hook removed it first.
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::NotFound
                ) => {}
            other => other?,
        }
    }
    Ok(())
}

/// Until when this device's hooks skip the server after it failed to answer.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Offline {
    until: Option<Timestamp>,
}

pub fn load_offline_until(home: &Home) -> Option<Timestamp> {
    load_named::<Offline>(home, "offline").until
}

pub fn save_offline_until(home: &Home, until: Option<Timestamp>) -> Result<()> {
    save_named(home, "offline", &Offline { until })
}

/// Generic named state file, for the daemon.
pub fn load_named<T: DeserializeOwned + Default>(home: &Home, name: &str) -> T {
    read_json(&home.state_dir().join(format!("{name}.json"))).unwrap_or_default()
}

pub fn save_named<T: Serialize>(home: &Home, name: &str, value: &T) -> Result<()> {
    write_json(&home.state_dir().join(format!("{name}.json")), value)
}

/// Removes a named state file; a missing file is fine.
pub fn remove_named(home: &Home, name: &str) -> Result<()> {
    remove_if_exists(&home.state_dir().join(format!("{name}.json")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stems_are_safe_and_distinct() {
        let a: AgentId = "t3:thread:x%3Ay".parse().unwrap();
        let b: AgentId = "t3:thread_x%3Ay".parse().unwrap();
        let stem = file_stem(&a);
        assert!(
            stem.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._".contains(&c))
        );
        assert_ne!(stem, file_stem(&b));
    }

    #[test]
    fn agent_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().to_path_buf());
        let agent: AgentId = "claude:s1".parse().unwrap();
        assert_eq!(load_agent(&home, &agent), AgentLocal::default());
        let state = AgentLocal {
            seq: Some(4),
            offline_notified: true,
            ..AgentLocal::default()
        };
        save_agent(&home, &agent, &state).unwrap();
        assert_eq!(load_agent(&home, &agent), state);
    }

    #[test]
    fn session_state_is_per_runtime_and_removable() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().to_path_buf());
        assert_eq!(load_session(&home, Runtime::Claude, "s/1"), None);
        let nested = SessionLocal {
            nested: true,
            t3_parent: Some("parent-1".into()),
        };
        save_session(&home, Runtime::Claude, "s/1", &nested).unwrap();
        assert_eq!(
            load_session(&home, Runtime::Claude, "s/1"),
            Some(nested.clone())
        );
        assert_eq!(load_session(&home, Runtime::Codex, "s/1"), None);
        remove_session(&home, Runtime::Claude, "s/1").unwrap();
        remove_session(&home, Runtime::Claude, "s/1").unwrap();
        assert_eq!(load_session(&home, Runtime::Claude, "s/1"), None);

        // Pruning keeps recent sessions and removes those older than the age.
        prune_sessions(&home, std::time::Duration::from_secs(60)).unwrap();
        save_session(&home, Runtime::Claude, "old", &nested).unwrap();
        save_session(&home, Runtime::Codex, "new", &nested).unwrap();
        let old = session_path(&home, Runtime::Claude, "old");
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
            .unwrap();
        prune_sessions(&home, std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(load_session(&home, Runtime::Claude, "old"), None);
        assert_eq!(load_session(&home, Runtime::Codex, "new"), Some(nested));
    }

    #[test]
    fn tool_starts_are_taken_once_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().to_path_buf());
        let at = chrono::Utc::now();
        let mark = ToolStart {
            at,
            git_dir: Some("/src/r/.git".into()),
            head: Some("4e1c2ab".into()),
        };
        // Nothing to prune or take before the first mark.
        prune_tool_starts(&home, std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(
            take_tool_start(&home, Runtime::Claude, "toolu_1").unwrap(),
            None
        );

        save_tool_start(&home, Runtime::Claude, "toolu_1", &mark).unwrap();
        assert_eq!(
            take_tool_start(&home, Runtime::Codex, "toolu_1").unwrap(),
            None
        );
        assert_eq!(
            take_tool_start(&home, Runtime::Claude, "toolu_1").unwrap(),
            Some(mark.clone())
        );
        assert_eq!(
            take_tool_start(&home, Runtime::Claude, "toolu_1").unwrap(),
            None
        );

        let bare = ToolStart {
            at,
            git_dir: None,
            head: None,
        };
        save_tool_start(&home, Runtime::Claude, "old/1", &bare).unwrap();
        save_tool_start(&home, Runtime::Codex, "new", &bare).unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(tool_start_path(&home, Runtime::Claude, "old/1"))
            .unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
            .unwrap();
        prune_tool_starts(&home, std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(
            take_tool_start(&home, Runtime::Claude, "old/1").unwrap(),
            None
        );
        assert_eq!(
            take_tool_start(&home, Runtime::Codex, "new").unwrap(),
            Some(bare)
        );
        // A mark saved before marks carried HEAD still reads.
        let path = tool_start_path(&home, Runtime::Claude, "older");
        std::fs::write(
            &path,
            format!("{{\"at\":{}}}", serde_json::to_string(&at).unwrap()),
        )
        .unwrap();
        assert_eq!(
            take_tool_start(&home, Runtime::Claude, "older").unwrap(),
            Some(ToolStart {
                at,
                git_dir: None,
                head: None,
            })
        );
    }
}
