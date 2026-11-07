-- What the user reads apart from the threads (docs/API.md, "Updates"): a done task's
-- summary for the user, coordinators' turn endings, and the user's seen cursor.
ALTER TABLE tasks ADD COLUMN summary TEXT;
CREATE INDEX tasks_done_at ON tasks (done_at) WHERE status = 'done';

-- The last message of a coordinator's turn, one row per board it coordinated then. The
-- newest rows per agent and board are kept.
CREATE TABLE said (
    id INTEGER PRIMARY KEY,
    agent TEXT NOT NULL,
    board TEXT NOT NULL,
    text TEXT NOT NULL,
    at INTEGER NOT NULL
) STRICT;
CREATE INDEX said_agent_board ON said (agent, board, id);
CREATE INDEX said_at ON said (at);

-- At most one row: everything up to `seen_at` has been seen by the user.
CREATE TABLE user_seen (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    seen_at INTEGER NOT NULL
) STRICT;
