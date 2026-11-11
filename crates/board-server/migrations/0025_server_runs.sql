-- One row per run of the server: when it started, and when its sweep last ran (every
-- minute, and once more at shutdown). The gap between one run's `alive_at` and the next
-- run's `started_at` is time the server was down, which the temporary-board sweep takes out
-- of a board's idle period. A restart adds one small row, so none is deleted.
CREATE TABLE server_runs (
    started_at INTEGER PRIMARY KEY,
    alive_at INTEGER NOT NULL
);
