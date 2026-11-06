//! Small JSON files in `$BOARD_HOME/state` and `$BOARD_HOME/cache`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use board_proto::{AgentId, Snapshot, Timestamp};
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
    /// The turn the `Stop` handoff reminder was last sent for.
    pub reminded_turn: Option<Timestamp>,
    /// Set while the server is unreachable and the cached snapshot was already injected.
    pub offline_notified: bool,
}

/// A file-system-safe name for an agent id.
pub fn file_stem(agent: &AgentId) -> String {
    agent
        .as_str()
        .bytes()
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
}
