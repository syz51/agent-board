use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use board_proto::{BoardSlug, DeviceName, Event, EventType, TaskId, Timestamp};
use chrono::DateTime;
use rusqlite::functions::FunctionFlags;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;

use crate::config::Settings;
use crate::error::{Error, Result};

/// Numbered schema migrations; `PRAGMA user_version` records how many have run.
pub(crate) const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_lowercase_repo_keys.sql"),
    include_str!("../migrations/0003_telegram_pairing.sql"),
    include_str!("../migrations/0004_events_actor.sql"),
    include_str!("../migrations/0005_telegram_chat_of_message.sql"),
    include_str!("../migrations/0006_task_not_before.sql"),
    include_str!("../migrations/0007_board_temporary.sql"),
    include_str!("../migrations/0008_touch_modified.sql"),
    include_str!("../migrations/0009_feedback.sql"),
    include_str!("../migrations/0010_user_updates.sql"),
    include_str!("../migrations/0011_query_indexes.sql"),
    include_str!("../migrations/0012_telegram_alerts.sql"),
    include_str!("../migrations/0013_telegram_stalled.sql"),
    include_str!("../migrations/0014_board_parallel_tasks.sql"),
    include_str!("../migrations/0015_api_errors_source.sql"),
    include_str!("../migrations/0016_feedback_resolved_report.sql"),
];

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Prepared statements kept per connection. The server caches more than 40 distinct
/// statements; rusqlite's default of 16 would evict and reparse them on every snapshot.
const STATEMENT_CACHE: usize = 128;

/// Read-only connections kept open beside the writer.
const READERS: usize = 4;

/// The board database. One writer connection serializes writes, each in a `BEGIN IMMEDIATE`
/// transaction. Reads run on a small pool of read-only connections, which WAL lets read
/// while a write is in progress. Committed events are broadcast to subscribers (SSE
/// streams, the notifier).
#[derive(Clone)]
pub struct Db {
    // Fields drop in order: the readers close before the writer, so the writer is the last
    // connection and checkpoints the WAL into the database file as it closes.
    readers: Arc<Readers>,
    writer: Arc<Mutex<Connection>>,
    events: broadcast::Sender<Event>,
    /// Read and write transactions begun, so tests can tell what a request cost.
    #[cfg(test)]
    transactions: Arc<[std::sync::atomic::AtomicUsize; 2]>,
}

impl Db {
    /// Opens (creating if needed) and migrates a database file, in WAL mode.
    pub fn open(path: &Path) -> Result<Self> {
        // The writer migrates and turns on WAL before any reader opens.
        let writer = open_file(path)?;
        let readers = Readers::open(path)?;
        let (events, _) = broadcast::channel(1024);
        Ok(Self {
            readers: Arc::new(readers),
            writer: Arc::new(Mutex::new(writer)),
            events,
            #[cfg(test)]
            transactions: Arc::default(),
        })
    }

    /// Read and write transactions begun since the database opened.
    #[cfg(test)]
    pub(crate) fn transactions(&self) -> (usize, usize) {
        use std::sync::atomic::Ordering::SeqCst;
        let [reads, writes] = &*self.transactions;
        (reads.load(SeqCst), writes.load(SeqCst))
    }

    #[cfg(test)]
    fn count_transaction(&self, write: bool) {
        self.transactions[usize::from(write)].fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// Creates a device (a user device when `user`) and returns its token, shown once.
    pub fn create_device(&self, name: &DeviceName, user: bool, now: Timestamp) -> Result<String> {
        let now = crate::clock::truncate(now);
        self.write(
            || now,
            &Settings::default(),
            |cx| crate::ops::devices::create(cx, name, user),
        )
        .map(|t| t.token)
    }

    /// Creates a one-time Telegram pairing code and returns it, shown once.
    pub fn create_telegram_pairing(&self, replace: bool, now: Timestamp) -> Result<String> {
        let now = crate::clock::truncate(now);
        self.write(
            || now,
            &Settings::default(),
            |cx| crate::ops::telegram::create_code(cx, replace),
        )
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        lock(&self.writer)
    }

    /// Runs `f` in an immediate (write) transaction, commits, then broadcasts the events it
    /// appended. Any error rolls everything back. `now` is read once the writer lock is
    /// held, so writes are stamped in the order they commit (the seen cursor relies on it).
    pub(crate) fn write<T>(
        &self,
        now: impl FnOnce() -> Timestamp,
        settings: &Settings,
        f: impl FnOnce(&mut Ctx<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.lock();
        #[cfg(test)]
        self.count_transaction(true);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut cx = Ctx {
            db: &tx,
            now: now(),
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

    /// Runs `f` in a read transaction, so it sees one consistent state: the last commit
    /// before it began. It runs on a read connection and never waits for the writer.
    pub(crate) fn read<T>(
        &self,
        now: Timestamp,
        settings: &Settings,
        f: impl FnOnce(&Ctx<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut conn = self.readers.lend();
        #[cfg(test)]
        self.count_transaction(false);
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic mid-transaction rolls the transaction back when it unwinds, so the
    // connection is still usable.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A fixed set of read-only connections, each lent to one read at a time.
struct Readers {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    returned: Condvar,
}

impl Readers {
    fn open(path: &Path) -> Result<Self> {
        let idle = (0..READERS)
            .map(|_| open_reader(path))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            path: path.to_owned(),
            idle: Mutex::new(idle),
            returned: Condvar::new(),
        })
    }

    /// Takes back a lent connection. One still in a transaction (its rollback failed) would
    /// fail every later read, so it is rolled back again, or else replaced.
    fn give_back(&self, mut conn: Connection) {
        if !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err() {
            match open_reader(&self.path) {
                Ok(fresh) => conn = fresh,
                Err(error) => {
                    // Keep the connection: a pool one short would leave reads waiting.
                    tracing::error!(%error, "replacing a read connection stuck in a transaction failed");
                }
            }
        }
        lock(&self.idle).push(conn);
        self.returned.notify_one();
    }

    /// Lends an idle connection, waiting for one when every one is in use.
    fn lend(&self) -> Lent<'_> {
        let mut idle = self
            .returned
            .wait_while(lock(&self.idle), |idle| idle.is_empty())
            .unwrap_or_else(PoisonError::into_inner);
        let conn = idle.pop().expect("waited for an idle connection");
        Lent {
            conn: Some(conn),
            readers: self,
        }
    }
}

/// A read connection on loan; it goes back to the pool when dropped.
struct Lent<'a> {
    conn: Option<Connection>,
    readers: &'a Readers,
}

impl std::ops::Deref for Lent<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("lent until dropped")
    }
}

impl std::ops::DerefMut for Lent<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("lent until dropped")
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.readers.give_back(conn);
        }
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

/// A read-only connection to a database the writer has opened and migrated, set up as the
/// writer is, so reads can call the same SQL functions.
fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        // As `Connection::open` does for the writer, so both read a `file:` URI path alike.
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure(&conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE);
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // SQL uses board-proto's rule, so a proposal is defined once.
    conn.create_scalar_function(
        "is_proposal_body",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |f| {
            // Borrowed: bodies run to 4,000 characters, and only the prefix is read.
            let body = f
                .get_raw(0)
                .as_str()
                .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
            Ok(board_proto::is_proposal_body(body))
        },
    )?;
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

/// SQL that is true when task `t` is a pending proposal (`board_proto::is_pending_proposal`).
pub(crate) fn proposal_sql(t: &str) -> String {
    format!(
        "({t}.status = 'open' AND {t}.auto = 0 AND is_proposal_body({t}.body) \
         AND NOT EXISTS (SELECT 1 FROM feedback pf WHERE pf.task = {t}.id))"
    )
}

/// SQL that is true when task `t` is ready: open, every blocker done, no live owner,
/// `not_before` (if any) passed, and not a pending proposal.
/// Expects the named parameters `:now` and `:cutoff`.
pub(crate) fn ready_sql(t: &str) -> String {
    format!(
        "({t}.status = 'open' \
         AND NOT EXISTS (SELECT 1 FROM task_blockers rb JOIN tasks rt ON rt.id = rb.blocker \
                         WHERE rb.task = {t}.id AND rt.status != 'done') \
         AND ({t}.owner IS NULL OR NOT {live}) \
         AND ({t}.not_before IS NULL OR {t}.not_before <= :now) \
         AND NOT {proposal})",
        live = live_sql(&format!("{t}.owner")),
        proposal = proposal_sql(t),
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
    fn items_resolved_before_resolved_report_take_the_reports_up_to_their_resolution() {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        for (index, migration) in MIGRATIONS[..15].iter().enumerate() {
            conn.execute_batch(migration).unwrap();
            conn.pragma_update(None, "user_version", index as i64 + 1)
                .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO boards (slug, title, goal, status, repos, autopilot, home_devices, \
             created_at, updated_at) VALUES ('f', 't', '', 'active', '[]', 0, '[]', 0, 0); \
             INSERT INTO tasks (id, board, number, title, body, acceptance, status, priority, \
             auto, created_by, version, created_at, updated_at) VALUES \
             ('f-1', 'f', 1, 't', '', '', 'done', 'p2', 0, 't3:a', 2, 0, 20), \
             ('f-2', 'f', 2, 't', '', '', 'done', 'p2', 0, 't3:a', 2, 0, 15), \
             ('f-3', 'f', 3, 't', '', '', 'open', 'p2', 0, 't3:a', 1, 0, 0); \
             INSERT INTO feedback (task, about, source, signature, reports, reporters, \
             last_reported_at, resolution, resolution_reason, resolved_at, resolved_by) VALUES \
             ('f-1', 'board', 'agent', 's1', 3, '[]', 30, 'declined', 'by design', 20, 'user'), \
             ('f-2', 'board', 'agent', 's2', 1, '[]', 10, 'declined', 'by design', 15, 'user'), \
             ('f-3', 'board', 'agent', 's3', 1, '[]', 10, NULL, NULL, NULL, NULL); \
             INSERT INTO feedback_reports (id, task, agent, at) VALUES \
             (1, 'f-1', 't3:a', 10), (2, 'f-2', 't3:a', 25), (3, 'f-1', 't3:b', 20), \
             (4, 'f-1', 't3:c', 30), (5, 'f-3', 't3:a', 10);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let resolved: Vec<(String, Option<i64>)> = conn
            .prepare("SELECT task, resolved_report FROM feedback ORDER BY task")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            resolved,
            [
                ("f-1".to_owned(), Some(3)),
                ("f-2".to_owned(), Some(0)),
                ("f-3".to_owned(), None)
            ]
        );
    }

    #[test]
    fn tasks_done_before_updates_count_as_seen() {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        for (index, migration) in MIGRATIONS[..9].iter().enumerate() {
            conn.execute_batch(migration).unwrap();
            conn.pragma_update(None, "user_version", index as i64 + 1)
                .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO boards (slug, title, goal, status, repos, autopilot, home_devices, \
             created_at, updated_at) VALUES ('b', 't', '', 'active', '[]', 0, '[]', 0, 0);",
        )
        .unwrap();
        for (number, status, done_at) in [
            (1, "done", Some(300)),
            (2, "done", Some(100)),
            (3, "open", None),
        ] {
            conn.execute(
                "INSERT INTO tasks (id, board, number, title, body, acceptance, status, priority, \
                 auto, created_by, version, created_at, updated_at, done_at) \
                 VALUES (?1, 'b', ?2, 't', '', '', ?3, 'p2', 1, 't3:a', 1, 0, 0, ?4)",
                rusqlite::params![format!("b-{number}"), number, status, done_at],
            )
            .unwrap();
        }
        migrate(&conn).unwrap();
        let seen: i64 = conn
            .query_row("SELECT seen_at FROM user_seen", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen, 300);

        // A new database has nothing to count as seen.
        let fresh = Connection::open_in_memory().unwrap();
        configure(&fresh).unwrap();
        migrate(&fresh).unwrap();
        let rows: i64 = fresh
            .query_row("SELECT count(*) FROM user_seen", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn boards_and_asks_stored_before_temporary_boards_are_kept() {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        for (index, migration) in MIGRATIONS[..6].iter().enumerate() {
            conn.execute_batch(migration).unwrap();
            conn.pragma_update(None, "user_version", index as i64 + 1)
                .unwrap();
        }
        conn.execute_batch(
            "INSERT INTO boards (slug, title, goal, status, repos, autopilot, home_devices, \
             created_at, updated_at) VALUES ('b', 't', '', 'closed', '[]', 0, '[]', 0, 0); \
             INSERT INTO asks (id, board, task, agent, kind, question, options, status, answer, \
             answered_via, created_at, answered_at, telegram_message_id, telegram_chat_id) \
             VALUES (5, 'b', 'b-1', 't3:a', 'decision', 'Ship?', '[\"yes\"]', 'answered', \
             'yes', 'telegram', 1, 2, 77, 88);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let temporary: bool = conn
            .query_row("SELECT temporary FROM boards WHERE slug = 'b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!temporary);
        // Asks keep every column, and a deleted ask's id is never handed out again.
        let ask: String = conn
            .query_row(
                "SELECT concat_ws(' ', id, board, task, agent, kind, question, options, status, \
                 answer, answered_via, created_at, answered_at, telegram_message_id, \
                 telegram_chat_id) FROM asks",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ask,
            "5 b b-1 t3:a decision Ship? [\"yes\"] answered yes telegram 1 2 77 88"
        );
        let new_ask = "INSERT INTO asks (board, agent, kind, question, options, status, \
                       created_at) VALUES ('b', 't3:a', 'action', 'Log in', '[]', 'open', 3) \
                       RETURNING id";
        conn.execute("DELETE FROM asks", []).unwrap();
        let id: i64 = conn.query_row(new_ask, [], |r| r.get(0)).unwrap();
        assert_eq!(id, 6);
        conn.execute("DELETE FROM asks", []).unwrap();
        let id: i64 = conn.query_row(new_ask, [], |r| r.get(0)).unwrap();
        assert_eq!(id, 7);
        let indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'asks_status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 1);
    }

    #[test]
    fn reads_run_on_read_only_connections_set_up_like_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("board.db")).unwrap();
        let settings = Settings::default();
        let now = DateTime::UNIX_EPOCH;
        // configure() ran: the SQL function the writer has is there.
        let proposal: bool = db
            .read(now, &settings, |cx| {
                Ok(cx
                    .db
                    .query_row("SELECT is_proposal_body('Proposal: x')", [], |r| r.get(0))?)
            })
            .unwrap();
        assert!(proposal);
        let write = db.read(now, &settings, |cx| {
            Ok(cx.db.execute("DELETE FROM devices", [])?)
        });
        assert!(
            write.is_err_and(|e| e.to_string().contains("readonly")),
            "a read wrote"
        );
    }

    #[test]
    fn a_read_connection_returned_in_a_transaction_is_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("board.db")).unwrap();
        {
            let conn = db.readers.lend();
            conn.execute_batch("BEGIN; SELECT COUNT(*) FROM boards;")
                .unwrap();
            assert!(!conn.is_autocommit());
        }
        assert!(lock(&db.readers.idle).iter().all(Connection::is_autocommit));
        for _ in 0..READERS * 2 {
            db.read(DateTime::UNIX_EPOCH, &Settings::default(), |cx| {
                cx.max_seq()
            })
            .unwrap();
        }
    }

    #[test]
    fn reads_beyond_the_pool_wait_for_a_connection() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("board.db")).unwrap();
        let (active, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..READERS * 3 {
                scope.spawn(|| {
                    db.read(DateTime::UNIX_EPOCH, &Settings::default(), |cx| {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        most.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(20));
                        active.fetch_sub(1, Ordering::SeqCst);
                        cx.max_seq()
                    })
                    .unwrap();
                });
            }
        });
        let most = most.into_inner();
        assert!((2..=READERS).contains(&most), "{most} reads at once");
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
