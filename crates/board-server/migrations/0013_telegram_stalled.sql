-- The stalled-board alert (docs/DESIGN.md, "Scheduled alerts"): one row per autopilot
-- board the last check saw stalled. `since` is when a check first saw it so, `seq` the
-- newest event then: a task moved into or out of doing after it starts the board over.
-- `sent_at` is set once the board was reported.
CREATE TABLE telegram_stalled (
    board TEXT PRIMARY KEY,
    seq INTEGER NOT NULL,
    since INTEGER NOT NULL,
    sent_at INTEGER
) STRICT;
