//! Local SQLite WAL database (`$BOARD_HOME/state/daemon.db`) for the device daemon
//! (`docs/SCHEDULER_DESIGN.md`, Section 4.1).
//!
//! Replaces the monolithic JSON `state/watch.json` and `state/worktrees.json` files with
//! transactional SQLite WAL storage (`PRAGMA synchronous=FULL`) so write-before-act intent
//! commits survive hard kills without losing or duplicating wake-ups, owed steps, or
//! worktree/worker records (`agent-board-679`, incorporating `agent-board-539`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context as _, Result};
use board_proto::{AgentId, BoardSlug, TaskId, Timestamp};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::config::Home;
use crate::daemon::{UnsureMessage, UnsureWakes, Wake, WakeQueue, WatchState};
use crate::worktree::{WORKTREES_STATE_NAME, WorktreeRecord, WorktreeStatus, WorktreeStore};

/// Filename of the local daemon SQLite database inside `$BOARD_HOME/state/`.
pub const DAEMON_DB_NAME: &str = "daemon.db";

/// Current `PRAGMA user_version` of `daemon.db`.
pub const SCHEMA_VERSION: u32 = 1;

const LEGACY_WATCH_NAME: &str = "watch";
const KV_MIGRATED_WATCH: &str = "migrated_watch_json";
const KV_MIGRATED_WORKTREES: &str = "migrated_worktrees_json";

/// Execution lifecycle state of a row in `local_runs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalRunState {
    /// Committed write-ahead intent before creating a worktree or spawning a runner.
    Intent,
    Starting,
    Running,
    Finished,
    Failed,
    Cancelled,
}

impl LocalRunState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "intent" => Self::Intent,
            "starting" => Self::Starting,
            "running" => Self::Running,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed | Self::Cancelled)
    }
}

/// Write-ahead intent and runtime tracking row in `daemon.db`'s `local_runs` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRunRecord {
    pub run_id: String,
    pub task_id: Option<TaskId>,
    pub board: BoardSlug,
    pub kind: String,
    pub runner: String,
    pub session_id: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub cost_units: u32,
    pub pid: Option<u32>,
    pub state: LocalRunState,
    pub lease_expires_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub meta_json: Option<String>,
}

/// Origin of a live entry in `resource_ledger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceOrigin {
    Managed,
    Interactive,
}

impl ResourceOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::Interactive => "interactive",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "interactive" => Self::Interactive,
            _ => Self::Managed,
        }
    }
}

/// One live weighted capacity entry in `daemon.db`'s `resource_ledger` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceEntry {
    pub entry_id: String,
    pub origin: ResourceOrigin,
    pub agent_id: Option<String>,
    pub run_id: Option<String>,
    pub cost_units: u32,
    pub updated_at: Timestamp,
}

/// Queued item in `daemon.db`'s `telemetry_outbox` table for crash-safe archival/flushing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryOutboxItem {
    pub id: i64,
    pub kind: String,
    pub payload_json: String,
    pub attempts: u32,
    pub next_retry_at: Timestamp,
    pub created_at: Timestamp,
}

/// Cloneable handle to `$BOARD_HOME/state/daemon.db`.
#[derive(Clone)]
pub struct LocalDb {
    path: Option<PathBuf>,
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for LocalDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalDb").field("path", &self.path).finish()
    }
}

impl LocalDb {
    /// Path of `daemon.db` under `home`.
    pub fn path_for(home: &Home) -> PathBuf {
        home.state_dir().join(DAEMON_DB_NAME)
    }

    /// Opens (or creates) `$BOARD_HOME/state/daemon.db`, configures WAL + `synchronous=FULL`,
    /// runs schema migrations, and performs one-time atomic migration from `watch.json` and
    /// `worktrees.json` if present.
    pub fn open_for_home(home: &Home) -> Result<Self> {
        let dir = home.state_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = Self::path_for(home);
        let db = Self::open_path(&path)?;
        db.migrate_legacy_files(home)?;
        Ok(db)
    }

    /// Opens `daemon.db` at `path` without running legacy JSON migration.
    pub fn open_path(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening SQLite database {}", path.display()))?;
        configure_and_init(&conn, true)?;
        Ok(Self {
            path: Some(path.to_path_buf()),
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Opens an isolated in-memory database (for tests or fallback).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory daemon.db")?;
        configure_and_init(&conn, false)?;
        Ok(Self {
            path: None,
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Returns the configured SQLite `journal_mode` and `user_version` (for verification).
    pub fn pragmas(&self) -> Result<(String, u32)> {
        let conn = self.lock();
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok((mode, version))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Performs one-time atomic migration of legacy `watch.json` and `worktrees.json` into
    /// `daemon.db`. Corrupt legacy files are preserved as `*.unreadable-<stamp>` with a
    /// warning matching the existing contract.
    fn migrate_legacy_files(&self, home: &Home) -> Result<()> {
        self.migrate_watch_json(home)?;
        self.migrate_worktrees_json(home)?;
        Ok(())
    }

    fn migrate_watch_json(&self, home: &Home) -> Result<()> {
        let path = home.state_dir().join(format!("{LEGACY_WATCH_NAME}.json"));
        if !path.exists() {
            return Ok(());
        }
        let already = {
            let conn = self.lock();
            get_kv_raw(&conn, KV_MIGRATED_WATCH)?.is_some()
        };
        if already {
            return Ok(());
        }
        let read = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice::<WatchState>(&bytes)?));
        let now = Utc::now();
        match read {
            Ok(mut watch_state) => {
                let migrated_now = {
                    let mut conn = self.lock();
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    if get_kv_raw(&tx, KV_MIGRATED_WATCH)?.is_some() {
                        false
                    } else {
                        seed_local_runs_from_watch_tx(&tx, &mut watch_state, now)?;
                        write_watch_state_tx(&tx, &watch_state, now)?;
                        set_kv_raw(&tx, KV_MIGRATED_WATCH, "1", ts_ms(now))?;
                        tx.commit()?;
                        true
                    }
                };
                if migrated_now {
                    let migrated = path.with_extension("json.migrated");
                    if let Err(err) = std::fs::rename(&path, &migrated) {
                        tracing::warn!(
                            "imported {} into daemon.db, but renaming it to {} failed: {err}",
                            path.display(),
                            migrated.display()
                        );
                    } else {
                        tracing::info!(
                            "migrated {} into {} (kept backup at {})",
                            path.display(),
                            Self::path_for(home).display(),
                            migrated.display()
                        );
                    }
                }
            }
            Err(err) => {
                let mark_unreadable = {
                    let mut conn = self.lock();
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    if get_kv_raw(&tx, KV_MIGRATED_WATCH)?.is_some() {
                        false
                    } else {
                        set_kv_raw(&tx, KV_MIGRATED_WATCH, "unreadable", ts_ms(now))?;
                        tx.commit()?;
                        true
                    }
                };
                if mark_unreadable {
                    let stamp = now.format("%Y%m%dT%H%M%SZ");
                    let copy = path.with_extension(format!("json.unreadable-{stamp}"));
                    match std::fs::rename(&path, &copy) {
                        Ok(()) => tracing::warn!(
                            "{}: cannot read it ({err:#}); starting fresh, with a copy in {}",
                            path.display(),
                            copy.display()
                        ),
                        Err(rename_err) => match std::fs::copy(&path, &copy) {
                            Ok(_) => tracing::warn!(
                                "{}: cannot read it ({err:#}); starting fresh, with a copy in {}",
                                path.display(),
                                copy.display()
                            ),
                            Err(copy_err) => tracing::warn!(
                                "{}: cannot read it ({err:#}) or copy it ({rename_err}; {copy_err}); starting fresh",
                                path.display()
                            ),
                        },
                    }
                }
            }
        }
        Ok(())
    }

    fn migrate_worktrees_json(&self, home: &Home) -> Result<()> {
        let path = home
            .state_dir()
            .join(format!("{WORKTREES_STATE_NAME}.json"));
        if !path.exists() {
            return Ok(());
        }
        let already = {
            let conn = self.lock();
            get_kv_raw(&conn, KV_MIGRATED_WORKTREES)?.is_some()
        };
        if already {
            return Ok(());
        }
        let now = Utc::now();
        let read = std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| parse_legacy_worktrees_json(&bytes));
        match read {
            Ok(records) => {
                let migrated_now = {
                    let mut conn = self.lock();
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    if get_kv_raw(&tx, KV_MIGRATED_WORKTREES)?.is_some() {
                        false
                    } else {
                        for record in &records {
                            upsert_worktree_tx(&tx, record)?;
                        }
                        set_kv_raw(&tx, KV_MIGRATED_WORKTREES, "1", ts_ms(now))?;
                        tx.commit()?;
                        true
                    }
                };
                if migrated_now {
                    let migrated = path.with_extension("json.migrated");
                    if let Err(err) = std::fs::rename(&path, &migrated) {
                        tracing::warn!(
                            "imported {} into daemon.db, but renaming it to {} failed: {err}",
                            path.display(),
                            migrated.display()
                        );
                    } else {
                        tracing::info!(
                            "migrated {} ({} records) into {} (kept backup at {})",
                            path.display(),
                            records.len(),
                            Self::path_for(home).display(),
                            migrated.display()
                        );
                    }
                }
            }
            Err(err) => {
                let mark_unreadable = {
                    let mut conn = self.lock();
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    if get_kv_raw(&tx, KV_MIGRATED_WORKTREES)?.is_some() {
                        false
                    } else {
                        set_kv_raw(&tx, KV_MIGRATED_WORKTREES, "unreadable", ts_ms(now))?;
                        tx.commit()?;
                        true
                    }
                };
                if mark_unreadable {
                    let stamp = now.format("%Y%m%dT%H%M%SZ");
                    let copy = path.with_extension(format!("json.unreadable-{stamp}"));
                    let _ = std::fs::rename(&path, &copy)
                        .or_else(|_| std::fs::copy(&path, &copy).map(|_| ()));
                    tracing::warn!(
                        "{}: cannot read it ({err:#}); starting fresh, with a copy in {}",
                        path.display(),
                        copy.display()
                    );
                }
            }
        }
        Ok(())
    }

    // -------------------------------------------------------------------------
    // WatchState transactional load / save
    // -------------------------------------------------------------------------

    /// Loads the daemon's [`WatchState`] from `event_cursor`, `sticky_wakes`, `unsure_sends`,
    /// and `kv_state`. Malformed individual entries are skipped leniently (matching legacy
    /// `watch.json` forward-compatibility guarantees).
    pub fn load_watch_state(&self) -> Result<WatchState> {
        let conn = self.lock();
        let last_seq: Option<i64> = conn
            .query_row("SELECT last_seq FROM event_cursor WHERE id = 1", [], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();

        // sticky_wakes -> WatchState::wakes
        let mut wakes = BTreeMap::new();
        {
            let mut stmt = conn.prepare("SELECT agent_id, queue_json FROM sticky_wakes")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (agent_raw, json) = row?;
                match (
                    agent_raw.parse::<AgentId>(),
                    serde_json::from_str::<WakeQueue>(&json),
                ) {
                    (Ok(agent), Ok(queue)) if !queue.wakes.is_empty() => {
                        wakes.insert(agent, queue);
                    }
                    (Ok(_), Ok(_)) => {}
                    _ => tracing::warn!(
                        "daemon.db sticky_wakes: skipped unreadable entry for {agent_raw}"
                    ),
                }
            }
        }

        // unsure_sends -> WatchState::unsure_wakes
        let mut unsure_wakes: BTreeMap<AgentId, UnsureWakes> = BTreeMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT agent_id, digest, wakes_json, since_ms, read_after_ms, failed_delay_secs \
                 FROM unsure_sends ORDER BY agent_id, since_ms, digest",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                ))
            })?;
            for row in rows {
                let (agent_raw, digest, wakes_json, since_ms, read_after_ms, failed_delay_secs) =
                    row?;
                let Ok(agent) = agent_raw.parse::<AgentId>() else {
                    tracing::warn!(
                        "daemon.db unsure_sends: skipped invalid agent id {agent_raw:?}"
                    );
                    continue;
                };
                let parsed_wakes: Vec<Wake> =
                    serde_json::from_str::<Vec<serde_json::Value>>(&wakes_json)
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|v| serde_json::from_value(v).ok())
                        .collect();
                if parsed_wakes.is_empty() {
                    continue;
                }
                let entry = unsure_wakes.entry(agent).or_default();
                entry.messages.push(UnsureMessage {
                    digest,
                    wakes: parsed_wakes,
                    since: ms_ts(since_ms),
                });
                if let Some(after) = read_after_ms.map(ms_ts) {
                    entry.read_after = Some(entry.read_after.map_or(after, |cur| cur.max(after)));
                }
                if let Some(delay) = failed_delay_secs {
                    entry.failed_delay_secs = Some(delay);
                }
            }
        }

        // Remaining structured fields in kv_state: decode each key independently so a single
        // malformed or future-version row never wipes unrelated workers, judges, or holds.
        let mut state = WatchState {
            last_seq,
            wakes,
            unsure_wakes,
            ..WatchState::default()
        };
        if let Some(raw) = get_kv_raw(&conn, "retry")? {
            match serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
                Ok(items) => {
                    for item in items {
                        match serde_json::from_value(item) {
                            Ok(ev) => state.retry.push(ev),
                            Err(err) => tracing::warn!(
                                "daemon.db kv_state[retry]: skipped unreadable event: {err}"
                            ),
                        }
                    }
                }
                Err(err) => tracing::warn!("daemon.db kv_state[retry]: unreadable ({err})"),
            }
        }
        if let Some(raw) = get_kv_raw(&conn, "usage_full_until")? {
            match serde_json::from_str(&raw) {
                Ok(val) => state.usage_full_until = val,
                Err(err) => {
                    tracing::warn!("daemon.db kv_state[usage_full_until]: unreadable ({err})")
                }
            }
        }
        if let Some(raw) = get_kv_raw(&conn, "goal_due")? {
            match serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
                Ok(items) => {
                    for item in items {
                        match serde_json::from_value(item) {
                            Ok(slug) => {
                                state.goal_due.insert(slug);
                            }
                            Err(err) => tracing::warn!(
                                "daemon.db kv_state[goal_due]: skipped unreadable board: {err}"
                            ),
                        }
                    }
                }
                Err(err) => tracing::warn!("daemon.db kv_state[goal_due]: unreadable ({err})"),
            }
        }
        for key in [
            "workers",
            "worker_retry",
            "worker_skips",
            "usage_stops",
            "holds",
            "hold_notes",
            "idle_notices",
            "owed",
            "judges",
            "judge_retry",
            "merges",
        ] {
            let Some(raw) = get_kv_raw(&conn, key)? else {
                continue;
            };
            let val = match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(val) => val,
                Err(err) => {
                    tracing::warn!("daemon.db kv_state[{key}]: unreadable JSON ({err})");
                    continue;
                }
            };
            let mut single = serde_json::Map::new();
            single.insert(key.to_owned(), val);
            let partial =
                match serde_json::from_value::<WatchState>(serde_json::Value::Object(single)) {
                    Ok(partial) => partial,
                    Err(err) => {
                        tracing::warn!("daemon.db kv_state[{key}]: skipped ({err})");
                        continue;
                    }
                };
            match key {
                "workers" => state.workers = partial.workers,
                "worker_retry" => state.worker_retry = partial.worker_retry,
                "worker_skips" => state.worker_skips = partial.worker_skips,
                "usage_stops" => state.usage_stops = partial.usage_stops,
                "holds" => state.holds = partial.holds,
                "hold_notes" => state.hold_notes = partial.hold_notes,
                "idle_notices" => state.idle_notices = partial.idle_notices,
                "owed" => state.owed = partial.owed,
                "judges" => state.judges = partial.judges,
                "judge_retry" => state.judge_retry = partial.judge_retry,
                "merges" => state.merges = partial.merges,
                _ => {}
            }
        }
        Ok(state)
    }

    /// Atomically persists the entire [`WatchState`] into `event_cursor`, `sticky_wakes`,
    /// `unsure_sends`, and `kv_state` inside one `BEGIN IMMEDIATE` transaction.
    pub fn save_watch_state(&self, state: &WatchState, now: Timestamp) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        write_watch_state_tx(&tx, state, now)?;
        tx.commit()?;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // local_runs (write-before-act intent log)
    // -------------------------------------------------------------------------

    /// Inserts or updates a [`LocalRunRecord`] durably in `local_runs`.
    pub fn upsert_local_run(&self, run: &LocalRunRecord) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_local_run_tx(&tx, run)?;
        tx.commit()?;
        Ok(())
    }

    /// Updates the state (and optional `session_id`, `worktree_path`, `branch`, `pid`) of an
    /// existing `local_runs` row.
    pub fn update_local_run_state(
        &self,
        run_id: &str,
        state: LocalRunState,
        session_id: Option<&str>,
        updated_at: Timestamp,
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE local_runs
                SET state = ?1,
                    session_id = COALESCE(?2, session_id),
                    updated_at = ?3
              WHERE run_id = ?4",
            params![state.as_str(), session_id, ts_ms(updated_at), run_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Updates a `local_runs` row's runtime state, optional `session_id`, and `pid` (`Some(pid)`
    /// sets the OS pid; `None` clears it when `clear_pid` is true, else preserves the prior pid).
    pub fn update_local_run_runtime(
        &self,
        run_id: &str,
        state: LocalRunState,
        session_id: Option<&str>,
        pid: Option<u32>,
        clear_pid: bool,
        updated_at: Timestamp,
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE local_runs
                SET state = ?1,
                    session_id = COALESCE(?2, session_id),
                    pid = CASE WHEN ?3 THEN ?4 ELSE COALESCE(?4, pid) END,
                    updated_at = ?5
              WHERE run_id = ?6",
            params![
                state.as_str(),
                session_id,
                clear_pid,
                pid.map(i64::from),
                ts_ms(updated_at),
                run_id,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reads the newest `local_runs` row matching `session_id`, for runner resume and status
    /// recovery after restarts.
    pub fn get_local_run_by_session(&self, session_id: &str) -> Result<Option<LocalRunRecord>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT run_id, task_id, board, kind, runner, session_id, worktree_path, branch, \
                    cost_units, pid, state, lease_expires_at, created_at, updated_at, meta_json \
               FROM local_runs WHERE session_id = ?1 \
              ORDER BY updated_at DESC, run_id DESC LIMIT 1",
            [session_id],
            local_run_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Marks every non-terminal `local_runs` row matching `(kind, task_id)` as `state`.
    pub fn finish_local_runs_for_task(
        &self,
        kind: &str,
        task_id: &TaskId,
        state: LocalRunState,
        updated_at: Timestamp,
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE local_runs
                SET state = ?1, updated_at = ?2
              WHERE kind = ?3 AND task_id = ?4
                AND state NOT IN ('finished', 'failed', 'cancelled')",
            params![state.as_str(), ts_ms(updated_at), kind, task_id.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Marks non-terminal `local_runs` rows matching `session_id` (or `board` + `kind`) as `state`.
    pub fn finish_local_run_by_session(
        &self,
        session_id: &str,
        state: LocalRunState,
        updated_at: Timestamp,
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE local_runs
                SET state = ?1, updated_at = ?2
              WHERE session_id = ?3
                AND state NOT IN ('finished', 'failed', 'cancelled')",
            params![state.as_str(), ts_ms(updated_at), session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reads one `local_runs` row by `run_id`.
    pub fn get_local_run(&self, run_id: &str) -> Result<Option<LocalRunRecord>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT run_id, task_id, board, kind, runner, session_id, worktree_path, branch, \
                    cost_units, pid, state, lease_expires_at, created_at, updated_at, meta_json \
               FROM local_runs WHERE run_id = ?1",
            [run_id],
            local_run_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Lists all non-terminal `local_runs` rows (oldest first), for crash reconciliation and
    /// tests.
    pub fn list_active_local_runs(&self) -> Result<Vec<LocalRunRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT run_id, task_id, board, kind, runner, session_id, worktree_path, branch, \
                    cost_units, pid, state, lease_expires_at, created_at, updated_at, meta_json \
               FROM local_runs \
              WHERE state NOT IN ('finished', 'failed', 'cancelled') \
              ORDER BY created_at ASC, run_id ASC",
        )?;
        let rows = stmt.query_map([], local_run_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Lists all `local_runs` rows (oldest first).
    pub fn list_local_runs(&self) -> Result<Vec<LocalRunRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT run_id, task_id, board, kind, runner, session_id, worktree_path, branch, \
                    cost_units, pid, state, lease_expires_at, created_at, updated_at, meta_json \
               FROM local_runs \
              ORDER BY created_at ASC, run_id ASC",
        )?;
        let rows = stmt.query_map([], local_run_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // -------------------------------------------------------------------------
    // resource_ledger (live weighted capacity accounting)
    // -------------------------------------------------------------------------

    /// Inserts or replaces one entry in `resource_ledger`.
    pub fn upsert_resource(&self, entry: &ResourceEntry) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO resource_ledger (entry_id, origin, agent_id, run_id, cost_units, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(entry_id) DO UPDATE SET
               origin = excluded.origin,
               agent_id = excluded.agent_id,
               run_id = excluded.run_id,
               cost_units = excluded.cost_units,
               updated_at = excluded.updated_at",
            params![
                entry.entry_id,
                entry.origin.as_str(),
                entry.agent_id,
                entry.run_id,
                i64::from(entry.cost_units),
                ts_ms(entry.updated_at),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Removes one entry from `resource_ledger`.
    pub fn remove_resource(&self, entry_id: &str) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM resource_ledger WHERE entry_id = ?1",
            [entry_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces all entries of `origin` (`Managed` or `Interactive`) atomically inside one
    /// transaction.
    pub fn replace_resources_by_origin(
        &self,
        origin: ResourceOrigin,
        entries: &[ResourceEntry],
    ) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM resource_ledger WHERE origin = ?1",
            [origin.as_str()],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO resource_ledger \
                 (entry_id, origin, agent_id, run_id, cost_units, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for entry in entries {
                stmt.execute(params![
                    entry.entry_id,
                    origin.as_str(),
                    entry.agent_id,
                    entry.run_id,
                    i64::from(entry.cost_units),
                    ts_ms(entry.updated_at),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Lists all live rows in `resource_ledger`.
    pub fn list_resources(&self) -> Result<Vec<ResourceEntry>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT entry_id, origin, agent_id, run_id, cost_units, updated_at \
               FROM resource_ledger ORDER BY entry_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ResourceEntry {
                entry_id: r.get(0)?,
                origin: ResourceOrigin::parse(&r.get::<_, String>(1)?),
                agent_id: r.get(2)?,
                run_id: r.get(3)?,
                cost_units: u32::try_from(r.get::<_, i64>(4)?.max(0)).unwrap_or(u32::MAX),
                updated_at: ms_ts(r.get(5)?),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Total weighted units currently active in `resource_ledger` (managed + interactive).
    pub fn active_units(&self) -> Result<u32> {
        let conn = self.lock();
        let sum: i64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_units), 0) FROM resource_ledger",
            [],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(sum.max(0)).unwrap_or(u32::MAX))
    }

    /// Computes available weighted units:
    /// `max_units.saturating_sub(reserve_units).saturating_sub(active_units)`.
    pub fn free_units(&self, max_units: u32, reserve_units: u32) -> Result<u32> {
        let active = self.active_units()?;
        Ok(max_units
            .saturating_sub(reserve_units)
            .saturating_sub(active))
    }

    // -------------------------------------------------------------------------
    // telemetry_outbox (crash-safe outbox for Slice 4.2 archival/events)
    // -------------------------------------------------------------------------

    /// Appends an entry to `telemetry_outbox` and returns its row id.
    pub fn enqueue_telemetry(&self, kind: &str, payload_json: &str, now: Timestamp) -> Result<i64> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ms = ts_ms(now);
        tx.execute(
            "INSERT INTO telemetry_outbox (kind, payload_json, attempts, next_retry_at, created_at)
             VALUES (?1, ?2, 0, ?3, ?4)",
            params![kind, payload_json, ms, ms],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// Returns up to `limit` due outbox entries (`next_retry_at <= now`), oldest first.
    pub fn due_telemetry(&self, now: Timestamp, limit: u32) -> Result<Vec<TelemetryOutboxItem>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, payload_json, attempts, next_retry_at, created_at \
               FROM telemetry_outbox \
              WHERE next_retry_at <= ?1 \
              ORDER BY created_at ASC, id ASC \
              LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![ts_ms(now), i64::from(limit)], |r| {
            Ok(TelemetryOutboxItem {
                id: r.get(0)?,
                kind: r.get(1)?,
                payload_json: r.get(2)?,
                attempts: u32::try_from(r.get::<_, i64>(3)?.max(0)).unwrap_or(u32::MAX),
                next_retry_at: ms_ts(r.get(4)?),
                created_at: ms_ts(r.get(5)?),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Removes a flushed outbox entry.
    pub fn mark_telemetry_sent(&self, id: i64) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM telemetry_outbox WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Increments `attempts` and pushes `next_retry_at` forward after a transient upload failure.
    pub fn mark_telemetry_retry(&self, id: i64, next_retry_at: Timestamp) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE telemetry_outbox
                SET attempts = attempts + 1, next_retry_at = ?1
              WHERE id = ?2",
            params![ts_ms(next_retry_at), id],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// SQLite-backed [`WorktreeStore`] persisting [`WorktreeRecord`] rows in `daemon.db`'s
/// `worktrees` table.
#[derive(Clone)]
pub struct SqliteWorktreeStore {
    db: LocalDb,
}

impl SqliteWorktreeStore {
    pub fn new(db: LocalDb) -> Self {
        Self { db }
    }

    pub fn open_for_home(home: &Home) -> Result<Self> {
        Ok(Self {
            db: LocalDb::open_for_home(home)?,
        })
    }
}

impl WorktreeStore for SqliteWorktreeStore {
    fn list(&self) -> Result<Vec<WorktreeRecord>> {
        let conn = self.db.lock();
        let mut stmt = conn.prepare(
            "SELECT id, repo_slug, repo_root, path, branch, base, status, \
                    created_at, last_used_at, cleanup_note \
               FROM worktrees ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], worktree_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn get(&self, id: &str) -> Result<Option<WorktreeRecord>> {
        let conn = self.db.lock();
        conn.query_row(
            "SELECT id, repo_slug, repo_root, path, branch, base, status, \
                    created_at, last_used_at, cleanup_note \
               FROM worktrees WHERE id = ?1",
            [id],
            worktree_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    fn upsert(&self, record: WorktreeRecord) -> Result<()> {
        let mut conn = self.db.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_worktree_tx(&tx, &record)?;
        tx.commit()?;
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        let mut conn = self.db.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM worktrees WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// Schema & internal helpers
// -----------------------------------------------------------------------------

fn configure_and_init(conn: &Connection, disk: bool) -> Result<()> {
    if disk {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .context("configuring daemon.db SQLite pragmas")?;
    } else {
        conn.execute_batch(
            "PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .context("configuring in-memory daemon.db SQLite pragmas")?;
    }

    let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < SCHEMA_VERSION {
        conn.execute_batch(SCHEMA_V1)
            .context("initializing daemon.db schema v1")?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    }
    Ok(())
}

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS local_runs (
    run_id           TEXT PRIMARY KEY,
    task_id          TEXT,
    board            TEXT NOT NULL,
    kind             TEXT NOT NULL,
    runner           TEXT NOT NULL,
    session_id       TEXT,
    worktree_path    TEXT,
    branch           TEXT,
    cost_units       INTEGER NOT NULL DEFAULT 2,
    pid              INTEGER,
    state            TEXT NOT NULL,
    lease_expires_at INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    meta_json        TEXT
);
CREATE INDEX IF NOT EXISTS idx_local_runs_state ON local_runs (state);
CREATE INDEX IF NOT EXISTS idx_local_runs_task ON local_runs (task_id);
CREATE INDEX IF NOT EXISTS idx_local_runs_session ON local_runs (session_id);

CREATE TABLE IF NOT EXISTS resource_ledger (
    entry_id   TEXT PRIMARY KEY,
    origin     TEXT NOT NULL CHECK (origin IN ('managed', 'interactive')),
    agent_id   TEXT,
    run_id     TEXT,
    cost_units INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_resource_ledger_origin ON resource_ledger (origin);

CREATE TABLE IF NOT EXISTS sticky_wakes (
    agent_id   TEXT PRIMARY KEY,
    queue_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS unsure_sends (
    agent_id          TEXT NOT NULL,
    digest            TEXT NOT NULL,
    wakes_json        TEXT NOT NULL,
    since_ms          INTEGER NOT NULL,
    read_after_ms     INTEGER,
    failed_delay_secs INTEGER,
    PRIMARY KEY (agent_id, digest)
);

CREATE TABLE IF NOT EXISTS worktrees (
    id           TEXT PRIMARY KEY,
    repo_slug    TEXT NOT NULL,
    repo_root    TEXT NOT NULL,
    path         TEXT NOT NULL,
    branch       TEXT NOT NULL,
    base         TEXT NOT NULL,
    status       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    cleanup_note TEXT
);

CREATE TABLE IF NOT EXISTS event_cursor (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    last_seq   INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS kv_state (
    key        TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS telemetry_outbox (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT NOT NULL,
    payload_json  TEXT NOT NULL,
    attempts      INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_telemetry_outbox_due ON telemetry_outbox (next_retry_at);
";

fn write_watch_state_tx(
    tx: &rusqlite::Transaction<'_>,
    state: &WatchState,
    now: Timestamp,
) -> Result<()> {
    let now_ms = ts_ms(now);

    // 1. event_cursor
    tx.execute(
        "INSERT INTO event_cursor (id, last_seq, updated_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET last_seq = excluded.last_seq, updated_at = excluded.updated_at",
        params![state.last_seq, now_ms],
    )?;

    // 2. sticky_wakes
    tx.execute("DELETE FROM sticky_wakes", [])?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO sticky_wakes (agent_id, queue_json, updated_at) VALUES (?1, ?2, ?3)",
        )?;
        for (agent, queue) in &state.wakes {
            if queue.wakes.is_empty() {
                continue;
            }
            let json = serde_json::to_string(queue)?;
            stmt.execute(params![agent.as_str(), json, now_ms])?;
        }
    }

    // 3. unsure_sends
    tx.execute("DELETE FROM unsure_sends", [])?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO unsure_sends \
             (agent_id, digest, wakes_json, since_ms, read_after_ms, failed_delay_secs) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(agent_id, digest) DO UPDATE SET \
               wakes_json = excluded.wakes_json, \
               since_ms = excluded.since_ms, \
               read_after_ms = excluded.read_after_ms, \
               failed_delay_secs = excluded.failed_delay_secs",
        )?;
        for (agent, unsure) in &state.unsure_wakes {
            let read_after = unsure.read_after.map(ts_ms);
            for msg in &unsure.messages {
                if msg.wakes.is_empty() {
                    continue;
                }
                let wakes_json = serde_json::to_string(&msg.wakes)?;
                stmt.execute(params![
                    agent.as_str(),
                    msg.digest,
                    wakes_json,
                    ts_ms(msg.since),
                    read_after,
                    unsure.failed_delay_secs,
                ])?;
            }
        }
    }

    // 4. Structured KV keys
    set_kv_json(tx, "retry", &state.retry, now_ms)?;
    set_kv_json(tx, "usage_full_until", &state.usage_full_until, now_ms)?;
    set_kv_json(tx, "workers", &state.workers, now_ms)?;
    set_kv_json(tx, "worker_retry", &state.worker_retry, now_ms)?;
    set_kv_json(tx, "worker_skips", &state.worker_skips, now_ms)?;
    set_kv_json(tx, "usage_stops", &state.usage_stops, now_ms)?;
    set_kv_json(tx, "holds", &state.holds, now_ms)?;
    set_kv_json(tx, "hold_notes", &state.hold_notes, now_ms)?;
    set_kv_json(tx, "idle_notices", &state.idle_notices, now_ms)?;
    set_kv_json(tx, "owed", &state.owed, now_ms)?;
    set_kv_json(tx, "judges", &state.judges, now_ms)?;
    set_kv_json(tx, "judge_retry", &state.judge_retry, now_ms)?;
    set_kv_json(tx, "merges", &state.merges, now_ms)?;
    set_kv_json(tx, "goal_due", &state.goal_due, now_ms)?;
    Ok(())
}

/// Seeds `local_runs` rows for any in-flight workers/judges imported during one-time
/// migration from `watch.json`, and stamps their generated `run_id` onto `state` so later
/// lifecycle transitions update the matching `local_runs` rows cleanly.
fn seed_local_runs_from_watch_tx(
    tx: &rusqlite::Transaction<'_>,
    state: &mut WatchState,
    now: Timestamp,
) -> Result<()> {
    for (task_id, worker) in &mut state.workers {
        let run_id = worker.run_id.clone().unwrap_or_else(|| {
            format!(
                "migrated-worker-{task_id}-{}",
                worker.started_at.timestamp_millis()
            )
        });
        worker.run_id = Some(run_id.clone());
        let run = LocalRunRecord {
            run_id,
            task_id: Some(task_id.clone()),
            board: task_id.board(),
            kind: "worker".to_owned(),
            runner: worker.runner.clone().unwrap_or_else(|| "t3".to_owned()),
            session_id: worker.thread.clone(),
            worktree_path: worker.worktree_path.clone(),
            branch: worker.branch.clone(),
            cost_units: worker
                .cost_units
                .unwrap_or(crate::daemon::device::STANDARD_RUN_UNITS),
            pid: None,
            state: if worker.thread.is_some() {
                LocalRunState::Running
            } else {
                LocalRunState::Intent
            },
            lease_expires_at: None,
            created_at: worker.started_at,
            updated_at: now,
            meta_json: serde_json::to_string(worker).ok(),
        };
        upsert_local_run_tx(tx, &run)?;
    }
    for (idx, judge) in state.judges.iter_mut().enumerate() {
        let run_id = judge.run_id.clone().unwrap_or_else(|| {
            format!(
                "migrated-judge-{}-{}-{idx}",
                judge.board,
                judge.started_at.timestamp_millis()
            )
        });
        judge.run_id = Some(run_id.clone());
        let run = LocalRunRecord {
            run_id,
            task_id: judge.tasks.first().cloned(),
            board: judge.board.clone(),
            kind: judge_kind_str(judge.kind).to_owned(),
            runner: judge.runner.clone().unwrap_or_else(|| "t3".to_owned()),
            session_id: judge.thread.clone(),
            worktree_path: judge.worktree_path.clone(),
            branch: None,
            cost_units: crate::daemon::device::cost_units_for_judge(judge.kind),
            pid: None,
            state: if judge.thread.is_some() {
                LocalRunState::Running
            } else {
                LocalRunState::Intent
            },
            lease_expires_at: None,
            created_at: judge.started_at,
            updated_at: now,
            meta_json: serde_json::to_string(judge).ok(),
        };
        upsert_local_run_tx(tx, &run)?;
    }
    Ok(())
}

fn judge_kind_str(kind: board_proto::JudgeKind) -> &'static str {
    match kind {
        board_proto::JudgeKind::Review => "review",
        board_proto::JudgeKind::Verdict => "verdict",
        board_proto::JudgeKind::Triage => "triage",
        board_proto::JudgeKind::Goal => "goal",
    }
}

fn upsert_local_run_tx(tx: &rusqlite::Transaction<'_>, run: &LocalRunRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO local_runs (
            run_id, task_id, board, kind, runner, session_id, worktree_path, branch,
            cost_units, pid, state, lease_expires_at, created_at, updated_at, meta_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(run_id) DO UPDATE SET
            task_id = excluded.task_id,
            board = excluded.board,
            kind = excluded.kind,
            runner = excluded.runner,
            session_id = excluded.session_id,
            worktree_path = excluded.worktree_path,
            branch = excluded.branch,
            cost_units = excluded.cost_units,
            pid = excluded.pid,
            state = excluded.state,
            lease_expires_at = excluded.lease_expires_at,
            updated_at = excluded.updated_at,
            meta_json = excluded.meta_json",
        params![
            run.run_id,
            run.task_id.as_ref().map(TaskId::as_str),
            run.board.as_str(),
            run.kind,
            run.runner,
            run.session_id,
            run.worktree_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            run.branch,
            i64::from(run.cost_units),
            run.pid.map(i64::from),
            run.state.as_str(),
            run.lease_expires_at.map(ts_ms),
            ts_ms(run.created_at),
            ts_ms(run.updated_at),
            run.meta_json,
        ],
    )?;
    Ok(())
}

fn local_run_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LocalRunRecord> {
    let task_id_raw: Option<String> = r.get(1)?;
    let board_raw: String = r.get(2)?;
    let worktree_raw: Option<String> = r.get(6)?;
    let cost_raw: i64 = r.get(8)?;
    let pid_raw: Option<i64> = r.get(9)?;
    let state_raw: String = r.get(10)?;
    let lease_raw: Option<i64> = r.get(11)?;
    let created_raw: i64 = r.get(12)?;
    let updated_raw: i64 = r.get(13)?;

    let board = board_raw.parse::<BoardSlug>().map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(err))
    })?;
    let task_id = match task_id_raw {
        Some(raw) => Some(raw.parse::<TaskId>().map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(err))
        })?),
        None => None,
    };
    Ok(LocalRunRecord {
        run_id: r.get(0)?,
        task_id,
        board,
        kind: r.get(3)?,
        runner: r.get(4)?,
        session_id: r.get(5)?,
        worktree_path: worktree_raw.map(PathBuf::from),
        branch: r.get(7)?,
        cost_units: u32::try_from(cost_raw.max(0)).unwrap_or(u32::MAX),
        pid: pid_raw.and_then(|p| u32::try_from(p).ok()),
        state: LocalRunState::parse(&state_raw),
        lease_expires_at: lease_raw.map(ms_ts),
        created_at: ms_ts(created_raw),
        updated_at: ms_ts(updated_raw),
        meta_json: r.get(14)?,
    })
}

fn upsert_worktree_tx(tx: &rusqlite::Transaction<'_>, record: &WorktreeRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO worktrees (
            id, repo_slug, repo_root, path, branch, base, status,
            created_at, last_used_at, cleanup_note
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
            repo_slug = excluded.repo_slug,
            repo_root = excluded.repo_root,
            path = excluded.path,
            branch = excluded.branch,
            base = excluded.base,
            status = excluded.status,
            created_at = excluded.created_at,
            last_used_at = excluded.last_used_at,
            cleanup_note = excluded.cleanup_note",
        params![
            record.id,
            record.repo_slug,
            record.repo_root.to_string_lossy().into_owned(),
            record.path.to_string_lossy().into_owned(),
            record.branch,
            record.base,
            worktree_status_str(record.status),
            ts_ms(record.created_at),
            ts_ms(record.last_used_at),
            record.cleanup_note,
        ],
    )?;
    Ok(())
}

fn worktree_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeRecord> {
    Ok(WorktreeRecord {
        id: r.get(0)?,
        repo_slug: r.get(1)?,
        repo_root: PathBuf::from(r.get::<_, String>(2)?),
        path: PathBuf::from(r.get::<_, String>(3)?),
        branch: r.get(4)?,
        base: r.get(5)?,
        status: parse_worktree_status(&r.get::<_, String>(6)?),
        created_at: ms_ts(r.get(7)?),
        last_used_at: ms_ts(r.get(8)?),
        cleanup_note: r.get(9)?,
    })
}

fn worktree_status_str(status: WorktreeStatus) -> &'static str {
    match status {
        WorktreeStatus::Active => "active",
        WorktreeStatus::Released => "released",
        WorktreeStatus::DirtySkipped => "dirty_skipped",
        WorktreeStatus::CleanupError => "cleanup_error",
        WorktreeStatus::Unknown => "unknown",
    }
}

fn parse_worktree_status(raw: &str) -> WorktreeStatus {
    match raw {
        "active" => WorktreeStatus::Active,
        "released" => WorktreeStatus::Released,
        "dirty_skipped" => WorktreeStatus::DirtySkipped,
        "cleanup_error" => WorktreeStatus::CleanupError,
        _ => WorktreeStatus::Unknown,
    }
}

fn parse_legacy_worktrees_json(bytes: &[u8]) -> Result<Vec<WorktreeRecord>> {
    let root: serde_json::Value = serde_json::from_slice(bytes)?;
    let Some(records) = root.get("records").and_then(|v| v.as_object()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (key, value) in records {
        match serde_json::from_value::<WorktreeRecord>(value.clone()) {
            Ok(rec) => out.push(rec),
            Err(err) => {
                tracing::warn!("worktree migration: skipped unreadable entry {key:?}: {err}");
            }
        }
    }
    Ok(out)
}

fn get_kv_raw(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value_json FROM kv_state WHERE key = ?1",
        [key],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn set_kv_raw(
    tx: &rusqlite::Transaction<'_>,
    key: &str,
    value_json: &str,
    now_ms: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO kv_state (key, value_json, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
        params![key, value_json, now_ms],
    )?;
    Ok(())
}

fn set_kv_json<T: Serialize>(
    tx: &rusqlite::Transaction<'_>,
    key: &str,
    value: &T,
    now_ms: i64,
) -> Result<()> {
    let json = serde_json::to_string(value)?;
    set_kv_raw(tx, key, &json, now_ms)
}

fn ts_ms(ts: Timestamp) -> i64 {
    ts.timestamp_millis()
}

fn ms_ts(ms: i64) -> Timestamp {
    Timestamp::from_timestamp_millis(ms).unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{IdleNotice, JudgeRetry, JudgeRun, Merge, Owed, WakeAbout, Worker};
    use board_proto::JudgeKind;
    use chrono::TimeDelta;

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn opens_with_wal_mode_and_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let db = LocalDb::open_for_home(&home).unwrap();
        let (mode, version) = db.pragmas().unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        assert_eq!(version, SCHEMA_VERSION);
        assert!(LocalDb::path_for(&home).exists());
    }

    #[test]
    fn migrates_watch_json_and_worktrees_json_once_and_renames_backups() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        std::fs::create_dir_all(home.state_dir()).unwrap();

        let agent: AgentId = "t3:worker-1".parse().unwrap();
        let task: TaskId = "factory-42".parse().unwrap();
        let board: BoardSlug = "factory".parse().unwrap();

        let mut legacy_watch = WatchState {
            last_seq: Some(123),
            usage_full_until: Some(at(3600)),
            ..WatchState::default()
        };
        legacy_watch.wakes.insert(
            agent.clone(),
            WakeQueue {
                wakes: vec![Wake {
                    board: board.clone(),
                    about: WakeAbout::Unblocked {
                        done: "factory-1".parse().unwrap(),
                        task: task.clone(),
                    },
                    queued_at: at(10),
                    forwarded: false,
                    resent: false,
                }],
                backoff: None,
            },
        );
        legacy_watch.unsure_wakes.insert(
            agent.clone(),
            UnsureWakes {
                messages: vec![UnsureMessage {
                    digest: "0123456789abcdef".to_owned(),
                    wakes: vec![Wake {
                        board: board.clone(),
                        about: WakeAbout::Answered {
                            ask: 7,
                            asker: agent.clone(),
                        },
                        queued_at: at(20),
                        forwarded: false,
                        resent: false,
                    }],
                    since: at(25),
                }],
                read_after: Some(at(85)),
                failed_delay_secs: Some(30),
            },
        );
        legacy_watch.workers.insert(
            task.clone(),
            Worker {
                thread: Some("thread-1".to_owned()),
                title: "factory-42 Migrate".to_owned(),
                branch: Some("task/factory-42".to_owned()),
                runner: Some("t3".to_owned()),
                worktree_id: Some("org-repo/run-1".to_owned()),
                worktree_path: Some(PathBuf::from("/tmp/wt/run-1")),
                run_id: None,
                cost_units: None,
                started_at: at(5),
                claimed: true,
                noted: true,
            },
        );
        legacy_watch.judges.push(JudgeRun {
            board: board.clone(),
            kind: JudgeKind::Review,
            tasks: vec![task.clone()],
            thread: Some("judge-1".to_owned()),
            runner: Some("t3".to_owned()),
            worktree_id: None,
            worktree_path: None,
            run_id: None,
            title: "review factory-42".to_owned(),
            started_at: at(15),
            registered: true,
            noted: true,
            worktree_note: Some("custom note".to_owned()),
        });
        legacy_watch.owed.push(Owed::Answer {
            board: board.clone(),
            ask: 9,
            asker: "claude:old".parse().unwrap(),
            check_gone: true,
            noted: false,
            at: at(30),
        });
        legacy_watch
            .holds
            .insert("worker factory-42".into(), at(35));
        legacy_watch.hold_notes.insert(task.clone(), at(36));
        legacy_watch.idle_notices.insert(
            task.clone(),
            IdleNotice {
                away: agent.clone(),
                since: at(1),
                noticed: at(40),
            },
        );
        legacy_watch.judge_retry.insert(
            "review:factory-42".into(),
            JudgeRetry {
                until: at(900),
                updated_at: Some(at(12)),
            },
        );
        legacy_watch.merges.insert(
            task.clone(),
            Merge {
                read_at: at(50),
                failed_head: Some("abc123".into()),
            },
        );
        legacy_watch.goal_due.insert(board.clone());
        legacy_watch.worker_retry.insert(task.clone(), at(500));
        legacy_watch.usage_stops.insert(agent.clone(), at(60));

        crate::state::save_named(&home, LEGACY_WATCH_NAME, &legacy_watch).unwrap();

        let wt_record = WorktreeRecord {
            id: "org-repo/run-1".to_owned(),
            repo_slug: "org-repo".to_owned(),
            repo_root: PathBuf::from("/src/repo"),
            path: PathBuf::from("/tmp/wt/run-1"),
            branch: "task/factory-42".to_owned(),
            base: "origin/main".to_owned(),
            status: WorktreeStatus::Active,
            created_at: at(5),
            last_used_at: at(5),
            cleanup_note: None,
        };
        crate::state::save_named(
            &home,
            WORKTREES_STATE_NAME,
            &serde_json::json!({
                "records": {
                    "org-repo/run-1": wt_record,
                }
            }),
        )
        .unwrap();

        let db = LocalDb::open_for_home(&home).unwrap();
        assert!(!home.state_dir().join("watch.json").exists());
        assert!(home.state_dir().join("watch.json.migrated").exists());
        assert!(!home.state_dir().join("worktrees.json").exists());
        assert!(home.state_dir().join("worktrees.json.migrated").exists());

        let loaded = db.load_watch_state().unwrap();
        // Migration stamps generated `run_id` onto migrated workers & judges so later
        // lifecycle transitions cleanly close their `local_runs` rows.
        assert!(loaded.workers[&task].run_id.is_some());
        assert!(loaded.judges[0].run_id.is_some());
        let mut expected = legacy_watch;
        expected.workers.get_mut(&task).unwrap().run_id = loaded.workers[&task].run_id.clone();
        expected.judges[0].run_id = loaded.judges[0].run_id.clone();
        assert_eq!(loaded, expected);

        // Migrated in-flight workers and judges are seeded into local_runs.
        let runs = db.list_active_local_runs().unwrap();
        assert_eq!(runs.len(), 2);
        assert!(
            runs.iter()
                .any(|r| r.kind == "worker" && r.branch.as_deref() == Some("task/factory-42"))
        );
        assert!(
            runs.iter()
                .any(|r| r.kind == "review" && r.session_id.as_deref() == Some("judge-1"))
        );

        // Worktrees were imported into the SQLite worktrees table.
        let store = SqliteWorktreeStore::new(db);
        let worktrees = store.list().unwrap();
        assert_eq!(worktrees, vec![wt_record]);
    }

    #[test]
    fn resource_ledger_tracks_weighted_managed_and_interactive_units() {
        let db = LocalDb::open_in_memory().unwrap();
        assert_eq!(db.active_units().unwrap(), 0);
        assert_eq!(db.free_units(12, 2).unwrap(), 10);

        // Managed heavy worker (4) + managed light verdict judge (1) + interactive standard (2).
        db.upsert_resource(&ResourceEntry {
            entry_id: "worker:factory-1".into(),
            origin: ResourceOrigin::Managed,
            agent_id: Some("t3:w1".into()),
            run_id: Some("run-1".into()),
            cost_units: 4,
            updated_at: at(1),
        })
        .unwrap();
        db.upsert_resource(&ResourceEntry {
            entry_id: "judge:verdict-1".into(),
            origin: ResourceOrigin::Managed,
            agent_id: Some("t3:j1".into()),
            run_id: Some("run-2".into()),
            cost_units: 1,
            updated_at: at(2),
        })
        .unwrap();
        db.upsert_resource(&ResourceEntry {
            entry_id: "interactive:t3:user-1".into(),
            origin: ResourceOrigin::Interactive,
            agent_id: Some("t3:user-1".into()),
            run_id: None,
            cost_units: 2,
            updated_at: at(3),
        })
        .unwrap();

        assert_eq!(db.active_units().unwrap(), 7);
        assert_eq!(db.free_units(12, 2).unwrap(), 3);
        // Saturates cleanly at 0 when over capacity.
        assert_eq!(db.free_units(6, 2).unwrap(), 0);

        // Replacing interactive entries leaves managed entries untouched.
        db.replace_resources_by_origin(ResourceOrigin::Interactive, &[])
            .unwrap();
        assert_eq!(db.active_units().unwrap(), 5);
        assert_eq!(db.free_units(12, 2).unwrap(), 5);
    }

    #[test]
    fn telemetry_outbox_enqueues_retries_and_removes() {
        let db = LocalDb::open_in_memory().unwrap();
        let id = db
            .enqueue_telemetry("run_archive", r#"{"run_id":"r1"}"#, at(10))
            .unwrap();
        let due = db.due_telemetry(at(10), 10).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, id);
        assert_eq!(due[0].attempts, 0);

        db.mark_telemetry_retry(id, at(10) + TimeDelta::seconds(60))
            .unwrap();
        assert!(db.due_telemetry(at(30), 10).unwrap().is_empty());
        let due_later = db.due_telemetry(at(70), 10).unwrap();
        assert_eq!(due_later.len(), 1);
        assert_eq!(due_later[0].attempts, 1);

        db.mark_telemetry_sent(id).unwrap();
        assert!(db.due_telemetry(at(100), 10).unwrap().is_empty());
    }

    #[test]
    fn corrupt_single_kv_key_and_duplicate_unsure_digest_never_wipe_or_break_state() {
        let db = LocalDb::open_in_memory().unwrap();
        let agent: AgentId = "t3:worker-dup".parse().unwrap();
        let task: TaskId = "factory-99".parse().unwrap();
        let board: BoardSlug = "factory".parse().unwrap();
        let wake = Wake {
            board,
            about: WakeAbout::Answered {
                ask: 1,
                asker: agent.clone(),
            },
            queued_at: at(10),
            forwarded: false,
            resent: false,
        };
        let mut state = WatchState {
            last_seq: Some(50),
            ..WatchState::default()
        };
        state.workers.insert(
            task.clone(),
            Worker {
                thread: Some("t1".into()),
                title: "factory-99 keep".into(),
                branch: Some("task/factory-99".into()),
                runner: Some("t3".into()),
                worktree_id: None,
                worktree_path: None,
                run_id: Some("run-99".into()),
                cost_units: Some(2),
                started_at: at(1),
                claimed: true,
                noted: true,
            },
        );
        // Duplicate digest in unsure_wakes must not fail save with UNIQUE constraint violation.
        state.unsure_wakes.insert(
            agent.clone(),
            UnsureWakes {
                messages: vec![
                    UnsureMessage {
                        digest: "deadbeef00000001".into(),
                        wakes: vec![wake.clone()],
                        since: at(10),
                    },
                    UnsureMessage {
                        digest: "deadbeef00000001".into(),
                        wakes: vec![wake],
                        since: at(11),
                    },
                ],
                read_after: None,
                failed_delay_secs: None,
            },
        );
        db.save_watch_state(&state, at(20)).unwrap();

        // Corrupt the `retry` kv_state row directly: workers and last_seq MUST still load!
        {
            let conn = db.lock();
            conn.execute(
                "UPDATE kv_state SET value_json = ?1 WHERE key = 'retry'",
                ["[{\"bad\":\"future-event\"}, \"not-an-event\"]"],
            )
            .unwrap();
        }
        let reloaded = db.load_watch_state().unwrap();
        assert_eq!(reloaded.last_seq, Some(50));
        assert!(reloaded.retry.is_empty());
        assert_eq!(reloaded.workers.len(), 1);
        assert!(reloaded.workers.contains_key(&task));
        assert_eq!(reloaded.unsure_wakes[&agent].messages.len(), 1);
    }
}
