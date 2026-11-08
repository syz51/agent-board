-- The stalled-board alert (docs/DESIGN.md, "Scheduled alerts"): one row per autopilot
-- board reported as stalled. `seq` is the board's newest event that moved a task into or
-- out of doing at the time, so the board is reported again only after such a move.
CREATE TABLE telegram_stalled (
    board TEXT PRIMARY KEY,
    seq INTEGER NOT NULL,
    sent_at INTEGER NOT NULL
) STRICT;
