//! Small JSON files in `$BOARD_HOME/state` and `$BOARD_HOME/cache`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use board_proto::{AgentId, Runtime, Snapshot, Timestamp};
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLocal {
    /// The session runs inside another agent session, so it does not take the T3 thread
    /// id it inherited (`identity::is_nested`).
    pub nested: bool,
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
    match std::fs::remove_file(session_path(home, runtime, session)) {
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
#[derive(Debug, Serialize, Deserialize)]
struct ToolStart {
    at: Timestamp,
}

/// Records when the tool call `tool_use` started. One file per call, so parallel calls
/// never write the same file.
pub fn save_tool_start(home: &Home, runtime: Runtime, tool_use: &str, at: Timestamp) -> Result<()> {
    write_json(&tool_start_path(home, runtime, tool_use), &ToolStart { at })
}

/// When the tool call `tool_use` started, forgetting it; `None` when no `PreToolUse` hook
/// recorded it.
pub fn take_tool_start(home: &Home, runtime: Runtime, tool_use: &str) -> Result<Option<Timestamp>> {
    let path = tool_start_path(home, runtime, tool_use);
    let start = read_json::<ToolStart>(&path).map(|s| s.at);
    match std::fs::remove_file(&path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
        _ => Ok(start),
    }
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
            match std::fs::remove_file(entry.path()) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err.into()),
                _ => {}
            }
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
        let nested = SessionLocal { nested: true };
        save_session(&home, Runtime::Claude, "s/1", &nested).unwrap();
        assert_eq!(load_session(&home, Runtime::Claude, "s/1"), Some(nested));
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
        // Nothing to prune or take before the first mark.
        prune_tool_starts(&home, std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(
            take_tool_start(&home, Runtime::Claude, "toolu_1").unwrap(),
            None
        );

        save_tool_start(&home, Runtime::Claude, "toolu_1", at).unwrap();
        assert_eq!(
            take_tool_start(&home, Runtime::Codex, "toolu_1").unwrap(),
            None
        );
        assert_eq!(
            take_tool_start(&home, Runtime::Claude, "toolu_1").unwrap(),
            Some(at)
        );
        assert_eq!(
            take_tool_start(&home, Runtime::Claude, "toolu_1").unwrap(),
            None
        );

        save_tool_start(&home, Runtime::Claude, "old/1", at).unwrap();
        save_tool_start(&home, Runtime::Codex, "new", at).unwrap();
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
            Some(at)
        );
    }
}
