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
    let Some(entries) = read_dir_if_exists(dir)? else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        // `false` when another hook removed it since the listing.
        if older_than(&path, age)? {
            remove_if_exists(&path)?;
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

/// The tasks `agent` marked done since the last call, oldest first, forgetting them. A
/// mark that cannot be removed is still returned, and read again by the next call.
pub fn take_done(home: &Home, agent: &AgentId) -> Result<Vec<DoneMark>> {
    let Some(entries) = read_dir_if_exists(&done_dir(home, agent))? else {
        return Ok(Vec::new());
    };
    let mut marks = Vec::new();
    for entry in entries {
        let path = entry?.path();
        // Not `write_atomic`'s temporary file, which a concurrent `record_done` renames.
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        if let Some(mark) = read_json::<DoneMark>(&path) {
            marks.push(mark);
        }
        if let Err(err) = remove_if_exists(&path) {
            tracing::debug!("removing done mark {}: {err:#}", path.display());
        }
    }
    marks.sort_by_key(|m| m.at);
    Ok(marks)
}

/// Removes done marks no `Stop` hook read for `age` (a session that ended mid-turn), and
/// agents' directories left empty for as long.
pub fn prune_done(home: &Home, age: std::time::Duration) -> Result<()> {
    let Some(entries) = read_dir_if_exists(&home.state_dir().join("done"))? else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        // Judged before its old marks go: a directory changed recently may be about to get
        // a mark (`record_done` creates it before writing).
        let idle = older_than(&dir, age)?;
        prune_dir(&dir, age)?;
        if idle {
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
    }
    Ok(())
}

/// The entries of `dir`; `None` when it does not exist.
fn read_dir_if_exists(dir: &Path) -> Result<Option<std::fs::ReadDir>> {
    match std::fs::read_dir(dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        entries => Ok(Some(entries?)),
    }
}

/// True when `path` was last modified more than `age` ago; false when it is gone.
fn older_than(path: &Path, age: std::time::Duration) -> Result<bool> {
    let modified = match std::fs::metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        metadata => metadata?.modified()?,
    };
    Ok(std::time::SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|elapsed| elapsed > age))
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

/// Like [`load_named`], for state that must not vanish unnoticed: a file that exists but
/// cannot be read is copied to `<name>.json.unreadable-<time>` with a warning, and the
/// state starts fresh.
pub fn load_named_keeping_unreadable<T: DeserializeOwned + Default>(home: &Home, name: &str) -> T {
    let path = home.state_dir().join(format!("{name}.json"));
    let read = std::fs::read(&path)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?));
    match read {
        Ok(value) => value,
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            T::default()
        }
        Err(err) => {
            let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
            let copy = path.with_extension(format!("json.unreadable-{stamp}"));
            match std::fs::copy(&path, &copy) {
                Ok(_) => tracing::warn!(
                    "{}: cannot read it ({err:#}); starting fresh, with a copy in {}",
                    path.display(),
                    copy.display()
                ),
                Err(copy_err) => tracing::warn!(
                    "{}: cannot read it ({err:#}) or copy it ({copy_err}); starting fresh",
                    path.display()
                ),
            }
            T::default()
        }
    }
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
    fn done_marks_are_taken_once_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().to_path_buf());
        let me: AgentId = "t3:thread-1".parse().unwrap();
        let peer: AgentId = "t3:thread-2".parse().unwrap();
        let t0 = chrono::Utc::now();
        let (a, b): (TaskId, TaskId) =
            ("factory-12".parse().unwrap(), "factory-9".parse().unwrap());
        assert_eq!(take_done(&home, &me).unwrap(), vec![]);
        prune_done(&home, std::time::Duration::from_secs(60)).unwrap();

        record_done(&home, &me, &a, t0 + chrono::Duration::seconds(1)).unwrap();
        record_done(&home, &me, &b, t0).unwrap();
        record_done(&home, &peer, &a, t0).unwrap();
        // A concurrent `record_done`'s temporary file is left alone.
        let tmp = done_dir(&home, &me).join("factory-13.json.tmp42");
        std::fs::write(&tmp, "{").unwrap();
        // A stray file beside the agents' directories does not stop pruning.
        std::fs::write(home.state_dir().join("done").join(".DS_Store"), "").unwrap();
        let taken = take_done(&home, &me).unwrap();
        assert_eq!(
            taken.iter().map(|m| m.task.as_str()).collect::<Vec<_>>(),
            ["factory-9", "factory-12"],
            "oldest first"
        );
        assert_eq!(take_done(&home, &me).unwrap(), vec![], "taken once");
        assert!(tmp.exists());

        // Fresh marks and directories stay; old ones go with their emptied directory.
        prune_done(&home, std::time::Duration::from_secs(60)).unwrap();
        assert!(done_dir(&home, &peer).exists());
        std::fs::remove_file(&tmp).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        for path in [
            done_dir(&home, &peer).join("factory-12.json"),
            done_dir(&home, &peer),
            done_dir(&home, &me),
        ] {
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        prune_done(&home, std::time::Duration::from_secs(60)).unwrap();
        assert!(!done_dir(&home, &peer).exists());
        assert!(!done_dir(&home, &me).exists());
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
