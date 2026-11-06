-- Times are milliseconds since the Unix epoch (UTC). JSON columns hold arrays or objects.

CREATE TABLE devices (
    name TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    user INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
) STRICT;

CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    handle TEXT NOT NULL UNIQUE,
    runtime TEXT NOT NULL,
    device TEXT NOT NULL,
    role TEXT NOT NULL,
    title TEXT,
    repo TEXT,
    branch TEXT,
    cwd TEXT,
    native_ids TEXT NOT NULL DEFAULT '{}',
    forked_from TEXT,
    state TEXT NOT NULL,
    paused_until INTEGER,
    last_seen INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    -- Snapshot cursor: the last event sequence this agent was shown.
    cursor INTEGER NOT NULL DEFAULT 0,
    -- Board counts last shown to this agent (JSON), so a delta reports only changes.
    last_counts TEXT
) STRICT;
CREATE INDEX agents_repo ON agents (repo, branch);

CREATE TABLE boards (
    slug TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    goal TEXT NOT NULL,
    status TEXT NOT NULL,
    repos TEXT NOT NULL,
    autopilot INTEGER NOT NULL,
    home_devices TEXT NOT NULL,
    coordinator TEXT,
    coordinator_device TEXT,
    coordinator_expires_at INTEGER,
    next_number INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE board_members (
    board TEXT NOT NULL REFERENCES boards (slug),
    agent TEXT NOT NULL REFERENCES agents (id),
    joined_at INTEGER NOT NULL,
    PRIMARY KEY (board, agent)
) STRICT;
CREATE INDEX board_members_agent ON board_members (agent);

CREATE TABLE tasks (
    id TEXT PRIMARY KEY,
    board TEXT NOT NULL REFERENCES boards (slug),
    number INTEGER NOT NULL,
    parent TEXT REFERENCES tasks (id),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    acceptance TEXT NOT NULL,
    status TEXT NOT NULL,
    priority TEXT NOT NULL,
    blocked_reason TEXT,
    owner TEXT REFERENCES agents (id),
    auto INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    version INTEGER NOT NULL,
    doing_since INTEGER,
    done_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX tasks_board ON tasks (board, status);
CREATE INDEX tasks_owner ON tasks (owner, status);

CREATE TABLE task_blockers (
    task TEXT NOT NULL REFERENCES tasks (id),
    blocker TEXT NOT NULL REFERENCES tasks (id),
    PRIMARY KEY (task, blocker)
) STRICT;
CREATE INDEX task_blockers_blocker ON task_blockers (blocker);

CREATE TABLE notes (
    id INTEGER PRIMARY KEY,
    task TEXT NOT NULL REFERENCES tasks (id),
    agent TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    at INTEGER NOT NULL
) STRICT;
CREATE INDEX notes_task ON notes (task);

CREATE TABLE evidence (
    id INTEGER PRIMARY KEY,
    task TEXT NOT NULL REFERENCES tasks (id),
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    exit_code INTEGER,
    agent TEXT NOT NULL,
    at INTEGER NOT NULL
) STRICT;
CREATE INDEX evidence_task ON evidence (task);

CREATE TABLE handoffs (
    agent TEXT PRIMARY KEY REFERENCES agents (id),
    now TEXT NOT NULL,
    next TEXT NOT NULL,
    blockers TEXT NOT NULL,
    version INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE leases (
    id INTEGER PRIMARY KEY,
    agent TEXT NOT NULL REFERENCES agents (id),
    task TEXT,
    repo TEXT NOT NULL,
    glob TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX leases_repo ON leases (repo, expires_at);
CREATE INDEX leases_agent ON leases (agent);

CREATE TABLE touches (
    agent TEXT NOT NULL REFERENCES agents (id),
    repo TEXT NOT NULL,
    path TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (agent, repo, path)
) STRICT;
CREATE INDEX touches_repo ON touches (repo, path, at);

CREATE TABLE asks (
    id INTEGER PRIMARY KEY,
    board TEXT NOT NULL REFERENCES boards (slug),
    task TEXT,
    agent TEXT NOT NULL,
    kind TEXT NOT NULL,
    question TEXT NOT NULL,
    options TEXT NOT NULL,
    status TEXT NOT NULL,
    answer TEXT,
    answered_via TEXT,
    created_at INTEGER NOT NULL,
    answered_at INTEGER,
    telegram_message_id INTEGER
) STRICT;
CREATE INDEX asks_status ON asks (status, board);

CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    board TEXT,
    actor TEXT NOT NULL,
    type TEXT NOT NULL,
    task TEXT,
    payload TEXT NOT NULL
) STRICT;
CREATE INDEX events_board ON events (board, seq);
CREATE INDEX events_type ON events (type, at);
CREATE INDEX events_task ON events (task, seq);

-- Refused claims leave no event (the transaction rolls back), so they are counted here.
CREATE TABLE claim_refusals (
    id INTEGER PRIMARY KEY,
    task TEXT NOT NULL,
    agent TEXT NOT NULL,
    at INTEGER NOT NULL
) STRICT;
CREATE INDEX claim_refusals_at ON claim_refusals (at);
