//! Small JSON files in `$BOARD_HOME/state` and `$BOARD_HOME/cache`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use board_proto::{AgentId, Overlap, Runtime, Snapshot, Timestamp};
use chrono::TimeDelta;
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
    /// What the last handoff reminder (`PostToolUse`) was for: the change it reported (or the turn,
    /// against a server that does not date changes), so each is reminded about once.
    pub reminded_for: Option<Timestamp>,
    /// Set while the server is unreachable and the cached snapshot was already injected.
    pub offline_notified: bool,
    /// The last snapshot said a paused board holds the agent (`Snapshot::held_by`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held: bool,
    /// When the latest board that holds the agent was paused (`Snapshot::held_since`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_since: Option<Timestamp>,
    /// The server's time of the current turn's prompt snapshot, to compare with
    /// `held_since` on one clock. Cleared at each prompt and when the turn ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_seen_at: Option<Timestamp>,
    /// The current turn began held: its prompt's snapshot told the agent. Used to decide a
    /// halt only without `held_since` and `turn_seen_at`. Cleared as `turn_seen_at` is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hold_told: bool,
    /// Overlap alerts shown after tool calls, newest last: see [`AgentLocal::first_sight`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlaps_shown: Vec<ShownOverlap>,
    /// Last heartbeat from a subagent's `PostToolUse`, apart from the agent's own
    /// (`last_heartbeat`), so a subagent the hold lets work on never takes the heartbeat
    /// that halts the agent's own held turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_heartbeat: Option<Timestamp>,
    /// Event sequence of the last alerts a subagent's `PostToolUse` fetched: the subagents'
    /// cursor, apart from the agent's own (`seq`), so the alerts a subagent's tool call
    /// fetched still reach the agent. A subagent asks from the later of the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_seq: Option<i64>,
}

/// An overlap alert the hooks showed: a peer's lease or edit of a path, as of `at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShownOverlap {
    pub path: String,
    pub agent: AgentId,
    pub via: String,
    /// The peer's lease or edit time the alert reported.
    pub at: Timestamp,
    pub shown_at: Timestamp,
}

/// An overlap alert repeats at most this often for one path, peer and kind.
pub const OVERLAP_REPEAT: TimeDelta = TimeDelta::hours(1);
/// Shown alerts are remembered this long, and at most this many.
const OVERLAP_MEMORY: TimeDelta = TimeDelta::days(1);
const OVERLAPS_KEPT: usize = 200;

impl AgentLocal {
    /// Forgets what the current turn knew of the hold: its prompt's time and whether the
    /// prompt showed the hold. For a new prompt, a new session, and the end of a turn.
    pub fn end_turn_hold(&mut self) {
        self.turn_seen_at = None;
        self.hold_told = false;
    }

    /// The agent is held, as of the last heartbeat, by a pause that came after its turn
    /// began, both as the server dates them: the heartbeat halts the turn. A turn begun
    /// while the board was already paused goes on, also once it claims the board's work
    /// mid-turn. Without either time (an older server, a prompt the server did not
    /// answer, a turn with no prompt), a turn not told of the hold at its prompt is halted.
    pub fn halted(&self) -> bool {
        if !self.held {
            return false;
        }
        match (self.held_since, self.turn_seen_at) {
            (Some(paused), Some(turn)) => paused > turn,
            _ => !self.hold_told,
        }
    }

    /// What the hold judges a foreground subagent by: the current turn, which runs it.
    pub fn turn_launch(&self) -> Launched {
        Launched {
            turn_seen_at: self.turn_seen_at,
        }
    }

    /// Whether to show `overlap` now, recording it when so. The same path, peer and kind
    /// show once an hour at most, and after that only for a newer lease or edit than the
    /// one shown, so an agent re-reads a file once per peer change instead of on every call.
    pub fn first_sight(&mut self, overlap: &Overlap, now: Timestamp) -> bool {
        self.overlaps_shown
            .retain(|s| now - s.shown_at < OVERLAP_MEMORY);
        let same = |s: &ShownOverlap| {
            s.path == overlap.path && s.agent == overlap.agent && s.via == overlap.via
        };
        if let Some(shown) = self.overlaps_shown.iter().find(|s| same(s))
            && (now - shown.shown_at < OVERLAP_REPEAT || overlap.at <= shown.at)
        {
            return false;
        }
        self.overlaps_shown.retain(|s| !same(s));
        self.overlaps_shown.push(ShownOverlap {
            path: overlap.path.clone(),
            agent: overlap.agent.clone(),
            via: overlap.via.clone(),
            at: overlap.at,
            shown_at: now,
        });
        let excess = self.overlaps_shown.len().saturating_sub(OVERLAPS_KEPT);
        self.overlaps_shown.drain(..excess);
        true
    }
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
    /// The T3 thread the facts below were read for; they hold only for that thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3_thread: Option<String>,
    /// The thread's title as T3 last gave it and the hook registered it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3_title: Option<String>,
    /// The thread's `https://` link in the T3 app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3_link: Option<String>,
    /// When a `Stop` last asked T3 for the thread's title; `None` before the session's
    /// first `Stop`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_checked_at: Option<Timestamp>,
    /// When T3 gave the registered thread facts above, so a background read that asked
    /// before then is older news.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t3_facts_at: Option<Timestamp>,
    /// When a hook last started a background read of the thread ([`ThreadRead`]), because
    /// T3 answered too slowly for the hook itself; `None` before the session's first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_read_at: Option<Timestamp>,
}

/// What a background read found of a session's T3 thread, kept until a hook registered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRead {
    /// The thread read; the facts hold only for it.
    pub thread: String,
    /// When the read asked T3.
    pub at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// The runtime as its state file names and `board hook --runtime` spell it.
pub fn runtime_name(runtime: Runtime) -> &'static str {
    match runtime {
        Runtime::Claude => "claude",
        Runtime::Codex => "codex",
        Runtime::Opencode => "opencode",
    }
}

fn session_path(home: &Home, runtime: Runtime, session: &str) -> PathBuf {
    home.state_dir().join("sessions").join(format!(
        "{}-{}.json",
        runtime_name(runtime),
        safe_stem(session)
    ))
}

fn thread_read_path(home: &Home, runtime: Runtime, session: &str) -> PathBuf {
    home.state_dir().join("thread-reads").join(format!(
        "{}-{}.json",
        runtime_name(runtime),
        safe_stem(session)
    ))
}

/// The session's finished background read, until a hook registered it.
pub fn load_thread_read(home: &Home, runtime: Runtime, session: &str) -> Option<ThreadRead> {
    read_json(&thread_read_path(home, runtime, session))
}

pub fn save_thread_read(
    home: &Home,
    runtime: Runtime,
    session: &str,
    read: &ThreadRead,
) -> Result<()> {
    write_json(&thread_read_path(home, runtime, session), read)
}

/// Forgets the session's background read; a missing file is fine.
pub fn remove_thread_read(home: &Home, runtime: Runtime, session: &str) -> Result<()> {
    remove_if_exists(&thread_read_path(home, runtime, session))
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

/// Forgets an ended session, its background read included; a missing file is fine. Both
/// are tried; the first failure is returned.
pub fn remove_session(home: &Home, runtime: Runtime, session: &str) -> Result<()> {
    let cache = remove_if_exists(&session_path(home, runtime, session));
    let read = remove_thread_read(home, runtime, session);
    cache.and(read)
}

/// Removes a file; a missing one is fine.
fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
        _ => Ok(()),
    }
}

/// Removes the cached decisions and background reads of sessions not written for `age`:
/// sessions that ended without a SessionEnd hook. A session that comes back decides again.
pub fn prune_sessions(home: &Home, age: std::time::Duration) -> Result<()> {
    let caches = prune_dir(&home.state_dir().join("sessions"), age);
    let reads = prune_dir(&home.state_dir().join("thread-reads"), age);
    caches.and(reads)
}

fn tool_start_path(home: &Home, runtime: Runtime, tool_use: &str) -> PathBuf {
    home.state_dir().join("tool-starts").join(format!(
        "{}-{}.json",
        runtime_name(runtime),
        safe_stem(tool_use)
    ))
}

/// Keeps the record of a launch the device limit refused, as `refused-launches/<name>.json`
/// in the state directory, and gives its path. These records are the trail of what an
/// agent asked for and did not get, so nothing prunes them.
pub fn save_refused_launch<T: Serialize>(home: &Home, name: &str, record: &T) -> Result<PathBuf> {
    let path = home
        .state_dir()
        .join("refused-launches")
        .join(format!("{}.json", safe_stem(name)));
    write_json(&path, record)?;
    Ok(path)
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

/// A subagent the agent launched in the background (Claude Code's `Agent` tool), which
/// outlives the launching turn and so what the agent's state knows of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Launched {
    /// The server's time of the launching turn's prompt snapshot (`AgentLocal::turn_seen_at`).
    pub turn_seen_at: Option<Timestamp>,
}

impl Launched {
    /// Whether a hold paused at `held_since` lets the subagent work on: the pause came
    /// before its launching turn began, so that turn's prompt showed the hold and the turn
    /// went on, which the hold allows only for the user's own request. Without either time
    /// it is held, since the turn's prompt may predate a later pause.
    pub fn passes(&self, held_since: Option<Timestamp>) -> bool {
        matches!((held_since, self.turn_seen_at), (Some(paused), Some(turn)) if paused <= turn)
    }
}

fn launched_path(home: &Home, agent: &AgentId, subagent: &str) -> PathBuf {
    home.state_dir().join("subagents").join(format!(
        "{}.{}.json",
        file_stem(agent),
        safe_stem(subagent)
    ))
}

/// Records subagent `subagent` of `agent`, in a file of its own: parallel launches each
/// run a hook, and the agent's state file is written whole by every hook.
pub fn save_launched(
    home: &Home,
    agent: &AgentId,
    subagent: &str,
    launched: &Launched,
) -> Result<()> {
    write_json(&launched_path(home, agent, subagent), launched)
}

/// The record of subagent `subagent`; `None` for a foreground subagent, never recorded.
pub fn load_launched(home: &Home, agent: &AgentId, subagent: &str) -> Option<Launched> {
    read_json(&launched_path(home, agent, subagent))
}

/// Removes records of subagents launched more than `age` ago.
pub fn prune_launched(home: &Home, age: std::time::Duration) -> Result<()> {
    prune_dir(&home.state_dir().join("subagents"), age)
}

/// The file stems of agent ids begin with their source and an encoded `:` ([`file_stem`]);
/// the named state files (the daemon's, the hooks' breaker) never do.
const AGENT_STEMS: [&str; 3] = ["t3_3a", "claude_3a", "codex_3a"];

/// Removes the hook state (`state/<agent>.json`) and cached snapshots (`cache/<agent>.json`)
/// not written for `age`: agents whose sessions ended long ago. An agent that comes back
/// starts fresh, with a full snapshot. Named state files are left alone.
pub fn prune_agents(home: &Home, age: std::time::Duration) -> Result<()> {
    for dir in [home.state_dir(), home.cache_dir()] {
        let Some(entries) = read_dir_if_exists(&dir)? else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            // Not `write_atomic`'s temporary files, which end in `.tmp<n>`.
            let agent = name.to_str().is_some_and(|name| {
                name.ends_with(".json") && AGENT_STEMS.iter().any(|s| name.starts_with(s))
            });
            if agent && entry.file_type()?.is_file() && older_than(&entry.path(), age)? {
                remove_if_exists(&entry.path())?;
            }
        }
    }
    Ok(())
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

/// Generic named state file, for the daemon and the hooks' breaker.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subagent_passes_only_a_pause_older_than_its_launching_turn() {
        let at = |s: &str| Some(s.parse::<Timestamp>().unwrap());
        let launched = Launched {
            turn_seen_at: at("2030-01-01T10:00:00Z"),
        };
        assert!(launched.passes(at("2030-01-01T09:00:00Z")));
        assert!(launched.passes(at("2030-01-01T10:00:00Z")));
        assert!(
            !launched.passes(at("2030-01-01T10:05:00Z")),
            "a later pause"
        );
        assert!(
            !launched.passes(None),
            "an older server: no time to compare"
        );
        let untimed = Launched { turn_seen_at: None };
        assert!(
            !untimed.passes(at("2030-01-01T09:00:00Z")),
            "a turn without a prompt"
        );
    }

    #[test]
    fn subagent_records_are_kept_apart_per_agent_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().to_path_buf());
        let parent: AgentId = "t3:parent".parse().unwrap();
        let other: AgentId = "t3:other".parse().unwrap();
        let launched = Launched {
            turn_seen_at: Some("2030-01-01T10:00:00Z".parse().unwrap()),
        };
        save_launched(&home, &parent, "a1", &launched).unwrap();
        assert_eq!(load_launched(&home, &parent, "a1"), Some(launched));
        assert_eq!(load_launched(&home, &other, "a1"), None);
        assert_eq!(
            load_launched(&home, &parent, "a2"),
            None,
            "a foreground subagent"
        );
        prune_launched(&home, std::time::Duration::ZERO).unwrap();
        assert_eq!(load_launched(&home, &parent, "a1"), None);
    }

    #[test]
    fn an_overlap_shows_once_an_hour_and_then_only_for_a_newer_change() {
        let start: Timestamp = "2026-11-08T10:00:00Z".parse().unwrap();
        let mins = TimeDelta::minutes;
        let overlap = |path: &str, agent: &str, via: &str, at: Timestamp| Overlap {
            path: path.to_owned(),
            agent: agent.parse().unwrap(),
            handle: agent.to_owned(),
            via: via.to_owned(),
            at,
        };
        let mut local = AgentLocal::default();
        let edit = overlap("src/lib.rs", "t3:peer", "touch", start - mins(3));
        assert!(local.first_sight(&edit, start));
        assert!(!local.first_sight(&edit, start + mins(1)), "same alert");
        // The peer edits again within the hour: still quiet.
        let again = overlap("src/lib.rs", "t3:peer", "touch", start + mins(10));
        assert!(!local.first_sight(&again, start + mins(11)));
        // Another path, peer or kind is its own alert.
        assert!(local.first_sight(&overlap("src/a.rs", "t3:peer", "touch", start), start));
        assert!(local.first_sight(&overlap("src/lib.rs", "t3:other", "touch", start), start));
        assert!(local.first_sight(&overlap("src/lib.rs", "t3:peer", "lease", start), start));
        // Past the hour: the newer edit shows; the same one again does not.
        assert!(local.first_sight(&again, start + mins(61)));
        assert!(!local.first_sight(&again, start + mins(130)));
        // A day later it is forgotten.
        assert!(local.first_sight(&again, start + TimeDelta::days(2)));
        // The memory stays bounded.
        for i in 0..(OVERLAPS_KEPT + 20) {
            let path = format!("src/{i}.rs");
            local.first_sight(&overlap(&path, "t3:peer", "touch", start), start);
        }
        assert_eq!(local.overlaps_shown.len(), OVERLAPS_KEPT);
    }

    /// An earlier build kept the `Stop` reminders' tasks in the agent's state; a file with
    /// them still loads whole, so the handoff reminder's record and the hold survive.
    #[test]
    fn agent_state_from_the_build_with_stop_reminders_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().to_path_buf());
        let me: AgentId = "t3:thread-1".parse().unwrap();
        let at = "2026-11-13T01:02:03Z";
        write_json(
            &agent_state_path(&home, &me),
            &serde_json::json!({
                "turn_started": at, "reminded_for": at, "offline_notified": false,
                "held": true, "done_reminded": ["factory-12"],
                "release_reminded": ["factory-13"], "filed_about": ["factory-14"],
            }),
        )
        .unwrap();
        let local = load_agent(&home, &me);
        let at: Timestamp = at.parse().unwrap();
        assert_eq!(local.turn_started, Some(at));
        assert_eq!(local.reminded_for, Some(at));
        assert!(local.held);
    }

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
            t3_thread: Some("thread-1".into()),
            t3_title: Some("Fix the parser".into()),
            t3_link: Some("https://app.example.test/threads/env-1/thread-1".into()),
            title_checked_at: Some(chrono::Utc::now()),
            t3_facts_at: Some(chrono::Utc::now()),
            thread_read_at: Some(chrono::Utc::now()),
        };
        save_session(&home, Runtime::Claude, "s/1", &nested).unwrap();
        let read = ThreadRead {
            thread: "thread-1".into(),
            at: chrono::Utc::now(),
            parent: None,
            title: Some("Fix the parser".into()),
            link: None,
        };
        save_thread_read(&home, Runtime::Claude, "s/1", &read).unwrap();
        assert_eq!(
            load_thread_read(&home, Runtime::Claude, "s/1"),
            Some(read.clone())
        );
        assert_eq!(load_thread_read(&home, Runtime::Codex, "s/1"), None);
        assert_eq!(
            load_session(&home, Runtime::Claude, "s/1"),
            Some(nested.clone())
        );
        assert_eq!(load_session(&home, Runtime::Codex, "s/1"), None);
        remove_session(&home, Runtime::Claude, "s/1").unwrap();
        remove_session(&home, Runtime::Claude, "s/1").unwrap();
        assert_eq!(load_session(&home, Runtime::Claude, "s/1"), None);
        assert_eq!(load_thread_read(&home, Runtime::Claude, "s/1"), None);

        // Pruning keeps recent sessions and removes those older than the age.
        prune_sessions(&home, std::time::Duration::from_secs(60)).unwrap();
        save_session(&home, Runtime::Claude, "old", &nested).unwrap();
        save_session(&home, Runtime::Codex, "new", &nested).unwrap();
        let old = session_path(&home, Runtime::Claude, "old");
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
            .unwrap();
        save_thread_read(&home, Runtime::Claude, "old", &read).unwrap();
        let old_read = thread_read_path(&home, Runtime::Claude, "old");
        let file = std::fs::File::options()
            .write(true)
            .open(&old_read)
            .unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
            .unwrap();
        save_thread_read(&home, Runtime::Codex, "new", &read).unwrap();
        prune_sessions(&home, std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(load_session(&home, Runtime::Claude, "old"), None);
        assert_eq!(load_session(&home, Runtime::Codex, "new"), Some(nested));
        assert_eq!(load_thread_read(&home, Runtime::Claude, "old"), None);
        assert_eq!(load_thread_read(&home, Runtime::Codex, "new"), Some(read));
    }

    #[test]
    fn agent_files_are_pruned_and_named_files_kept() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().to_path_buf());
        let age = std::time::Duration::from_secs(60);
        // Nothing to prune before the first hook.
        prune_agents(&home, age).unwrap();

        let snap: Snapshot = serde_json::from_value(serde_json::json!({
            "kind": "full", "seq": 3, "at": chrono::Utc::now(), "text": "x"
        }))
        .unwrap();
        let ids = ["t3:old", "claude:old", "codex:old", "t3:new"].map(|id| id.parse().unwrap());
        for agent in &ids {
            save_agent(&home, agent, &AgentLocal::default()).unwrap();
            save_cached_snapshot(&home, agent, &snap).unwrap();
        }
        save_named(&home, "watch", &1).unwrap();
        save_named(&home, "offline", &1).unwrap();
        let tmp = home.state_dir().join("t3_3aold.json.tmp42");
        std::fs::write(&tmp, "{").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
        let backdate = |path: &Path| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        };
        for agent in &ids[..3] {
            backdate(&agent_state_path(&home, agent));
            backdate(&cache_path(&home, agent));
        }
        for name in ["watch", "offline"] {
            backdate(&home.state_dir().join(format!("{name}.json")));
        }
        backdate(&tmp);

        prune_agents(&home, age).unwrap();
        for agent in &ids[..3] {
            assert!(!agent_state_path(&home, agent).exists(), "{agent}");
            assert!(!cache_path(&home, agent).exists(), "{agent}");
        }
        assert!(agent_state_path(&home, &ids[3]).exists());
        assert!(load_cached_snapshot(&home, &ids[3]).is_some());
        assert!(home.state_dir().join("watch.json").exists());
        assert!(home.state_dir().join("offline.json").exists());
        assert!(tmp.exists(), "a write in progress is left alone");
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
