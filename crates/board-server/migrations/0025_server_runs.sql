-- One row per run of the server: when it started, and when its sweep last ran (every
-- minute, and once more at shutdown). The gap between one run's `alive_at` and the next
-- run's `started_at` is time the server was down, which the temporary-board sweep takes out
-- of a board's idle period. A restart adds one small row, so none is deleted.
CREATE TABLE server_runs (
    started_at INTEGER PRIMARY KEY,
    alive_at INTEGER NOT NULL
);

-- The server that ran before this table existed was alive at its newest event at the
-- latest, so the first run with the table takes the time since then as time down, which
-- keeps a board rather than deletes it. A database without events has no boards.
INSERT INTO server_runs (started_at, alive_at)
SELECT MAX(at), MAX(at) FROM events HAVING MAX(at) IS NOT NULL;
