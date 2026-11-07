-- Telegram alerts sent on a schedule (docs/DESIGN.md, "Notifications").

-- One row per task the notifier reported as stuck: `seq` is the task's newest event (not
-- by the server) at the time, so the task is reported again only after it changes and then
-- goes quiet again. Rows of tasks no longer in doing or review are deleted.
CREATE TABLE telegram_stuck (
    task TEXT PRIMARY KEY,
    seq INTEGER NOT NULL,
    sent_at INTEGER NOT NULL
) STRICT;

-- At most one row: the local date (YYYY-MM-DD) of the last daily digest.
CREATE TABLE telegram_digest (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    day TEXT NOT NULL
) STRICT;
