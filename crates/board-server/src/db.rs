use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use board_proto::{BoardSlug, DeviceName, Event, EventType, TaskId, Timestamp};
use chrono::DateTime;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;

use crate::config::Settings;
use crate::error::{Error, Result};

/// Numbered schema migrations; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_lowercase_repo_keys.sql"),
    include_str!("../migrations/0003_telegram_pairing.sql"),
    include_str!("../migrations/0004_events_actor.sql"),
];

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The board database: one SQLite connection that every request uses in turn, so writes
/// are serialized, each in a `BEGIN IMMEDIATE` transaction. Committed events are
/// broadcast to subscribers (SSE streams, the notifier).
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    events: broadcast::Sender<Event>,
}

impl Db {
    /// Opens (creating if needed) and migrates a database file, in WAL mode.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self::from_connection(open_file(path)?))
    }

    /// A fresh, migrated in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate(&conn)?;
        Ok(Self::from_connection(conn))
    }

    fn from_connection(conn: Connection) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            conn: Arc::new(Mutex::new(conn)),
            events,
        }
    }

    /// Creates a device (a user device when `user`) and returns its token, shown once.
    pub fn create_device(&self, name: &DeviceName, user: bool, now: Timestamp) -> Result<String> {
        let now = crate::clock::truncate(now);
        self.write(now, &Settings::default(), |cx| {
            crate::ops::devices::create(cx, name, user)
        })
        .map(|t| t.token)
    }

    /// Creates a one-time Telegram pairing code and returns it, shown once.
    pub fn create_telegram_pairing(&self, replace: bool, now: Timestamp) -> Result<String> {
        let now = crate::clock::truncate(now);
        self.write(now, &Settings::default(), |cx| {
            crate::ops::telegram::create_code(cx, replace)
        })
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic mid-transaction rolls the transaction back when it unwinds, so the
        // connection is still usable.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` in an immediate (write) transaction, commits, then broadcasts the events it
    /// appended. Any error rolls everything back.
    pub(crate) fn write<T>(
        &self,
        now: Timestamp,
        settings: &Settings,
        f: impl FnOnce(&mut Ctx<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut cx = Ctx {
            db: &tx,
            now,
            s: settings,
            events: Vec::new(),
        };
        let out = f(&mut cx)?;
        let events = std::mem::take(&mut cx.events);
        tx.commit()?;
        for event in events {
            // No subscribers is fine.
            let _ = self.events.send(event);
        }
        Ok(out)
    }

    /// Runs `f` in a read transaction, so it sees one consistent state.
    pub(crate) fn read<T>(
        &self,
        now: Timestamp,
        settings: &Settings,
        f: impl FnOnce(&Ctx<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let cx = Ctx {
            db: &tx,
            now,
            s: settings,
            events: Vec::new(),
        };
        let out = f(&cx)?;
        drop(cx);
        tx.finish()?;
        Ok(out)
    }
}

pub(crate) fn open_file(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let known = MIGRATIONS.len() as i64;
    if current > known {
        return Err(Error::Config(format!(
            "database schema version {current} is newer than this server ({known}); upgrade the server"
        )));
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

/// One transaction's view of the database, with the request time and settings.
pub(crate) struct Ctx<'a> {
    pub db: &'a Connection,
    pub now: Timestamp,
    pub s: &'a Settings,
    events: Vec<Event>,
}

impl Ctx<'_> {
    pub fn now_ms(&self) -> i64 {
        self.now.timestamp_millis()
    }

    /// Owners and peers last seen before this are stale.
    pub fn stale_cutoff(&self) -> i64 {
        self.now_ms() - self.s.stale_ms()
    }

    /// Appends an event, to be broadcast once the transaction commits.
    pub fn emit(
        &mut self,
        board: Option<&BoardSlug>,
        actor: &str,
        event_type: EventType,
        task: Option<&TaskId>,
        payload: serde_json::Value,
    ) -> Result<i64> {
        self.db.execute(
            "INSERT INTO events (at, board, actor, type, task, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                self.now_ms(),
                board.map(BoardSlug::as_str),
                actor,
                enum_str(&event_type),
                task.map(TaskId::as_str),
                payload.to_string(),
            ],
        )?;
        let seq = self.db.last_insert_rowid();
        self.events.push(Event {
            seq,
            at: self.now,
            board: board.cloned(),
            actor: actor.to_owned(),
            event_type,
            task: task.cloned(),
            payload,
        });
        Ok(seq)
    }

    pub fn max_seq(&self) -> Result<i64> {
        Ok(self
            .db
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    pub fn exists(&self, sql: &str, params: impl rusqlite::Params) -> Result<bool> {
        Ok(self
            .db
            .query_row(sql, params, |_| Ok(()))
            .optional()?
            .is_some())
    }
}

/// The `snake_case` wire name of a unit enum value.
pub(crate) fn enum_str<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => unreachable!("unit enums serialize as strings"),
    }
}

/// Parses a stored enum value; a bad value means a corrupt row.
pub(crate) fn parse_enum<T: DeserializeOwned>(text: &str) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

pub(crate) fn parse_json<T: DeserializeOwned>(text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// Parses a stored id; a bad value means a corrupt row.
pub(crate) fn parse_id<T: std::str::FromStr>(text: &str) -> rusqlite::Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    text.parse().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

pub(crate) fn ts(ms: i64) -> Timestamp {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

/// SQL that is true when the agent in column `col` is live: seen within the stale period,
/// or paused until a time still ahead. Expects the named parameters `:now` and `:cutoff`.
pub(crate) fn live_sql(col: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM agents la WHERE la.id = {col} \
         AND (la.last_seen > :cutoff OR (la.state = 'paused' AND la.paused_until > :now)))"
    )
}

/// SQL that is true when task `t` is ready: open, every blocker done, no live owner.
/// Expects the named parameters `:now` and `:cutoff`.
pub(crate) fn ready_sql(t: &str) -> String {
    format!(
        "({t}.status = 'open' \
         AND NOT EXISTS (SELECT 1 FROM task_blockers rb JOIN tasks rt ON rt.id = rb.blocker \
                         WHERE rb.task = {t}.id AND rt.status != 'done') \
         AND ({t}.owner IS NULL OR NOT {live}))",
        live = live_sql(&format!("{t}.owner"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_once_and_set_user_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.db");
        drop(open_file(&path).unwrap());
        let conn = open_file(&path).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn migration_lowercases_stored_repo_keys() {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO boards (slug, title, goal, status, repos, autopilot, home_devices, \
             created_at, updated_at) VALUES ('b', 't', '', 'active', \
             '[\"github.com/MyOrg/Repo\",\"github.com/o/x\",\"github.com/myorg/repo\"]', \
             0, '[]', 0, 0); \
             INSERT INTO agents (id, handle, runtime, device, role, repo, state, last_seen, \
             created_at) VALUES ('claude:a', 'claude:a', 'claude', 'd', 'worker', \
             'github.com/MyOrg/Repo', 'idle', 0, 0); \
             INSERT INTO leases (agent, repo, glob, expires_at, created_at) \
             VALUES ('claude:a', 'github.com/MyOrg/Repo', 'src/**', 1, 0); \
             INSERT INTO touches (agent, repo, path, at) VALUES \
             ('claude:a', 'github.com/MyOrg/Repo', 'a.rs', 5), \
             ('claude:a', 'github.com/myorg/repo', 'a.rs', 3), \
             ('claude:a', 'github.com/MyOrg/Repo', 'b.rs', 1);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let one = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, String>(0)).unwrap();
        assert_eq!(
            one("SELECT repos FROM boards"),
            r#"["github.com/myorg/repo","github.com/o/x"]"#
        );
        assert_eq!(one("SELECT repo FROM agents"), "github.com/myorg/repo");
        assert_eq!(one("SELECT repo FROM leases"), "github.com/myorg/repo");
        assert_eq!(
            one(
                "SELECT group_concat(repo || ' ' || path || ' ' || at, '; ') FROM \
                 (SELECT * FROM touches ORDER BY path)"
            ),
            "github.com/myorg/repo a.rs 5; github.com/myorg/repo b.rs 1"
        );
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("board.db");
        let conn = open_file(&path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(open_file(&path).is_err());
    }
}
