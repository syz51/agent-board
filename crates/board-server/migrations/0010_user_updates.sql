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
-- An existing board's history predates updates: count it as seen, so the user starts
-- from what finishes next rather than from every task ever done.
INSERT INTO user_seen (id, seen_at)
SELECT 1, done_at FROM tasks WHERE done_at IS NOT NULL ORDER BY done_at DESC LIMIT 1;
