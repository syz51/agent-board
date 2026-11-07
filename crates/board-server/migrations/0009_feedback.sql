-- Feedback items: tasks on the feedback board with their report counts and resolution.
-- Reports with an equal signature add to the newest item with it while that item is live.
CREATE TABLE feedback (
    task TEXT PRIMARY KEY REFERENCES tasks (id),
    about TEXT NOT NULL,
    activity TEXT,
    source TEXT NOT NULL,
    signature TEXT NOT NULL,
    reports INTEGER NOT NULL,
    -- JSON array of agent ids, oldest first, capped.
    reporters TEXT NOT NULL,
    last_reported_at INTEGER NOT NULL,
    -- When a repeat report last added a note, so notes come at most once an hour.
    last_noted_at INTEGER,
    resolution TEXT,
    resolution_task TEXT,
    resolution_reason TEXT,
    resolved_at INTEGER,
    resolved_by TEXT,
    recurs TEXT
) STRICT;
CREATE INDEX feedback_signature ON feedback (signature);

-- One row per report, for the stats.
CREATE TABLE feedback_reports (
    id INTEGER PRIMARY KEY,
    task TEXT NOT NULL,
    agent TEXT NOT NULL,
    at INTEGER NOT NULL
) STRICT;
CREATE INDEX feedback_reports_at ON feedback_reports (at);
CREATE INDEX feedback_reports_task ON feedback_reports (task);

-- Agent-facing API errors, normalized, so repeated ones file feedback (source `signal`).
-- Pruned once older than the signal window.
CREATE TABLE api_errors (
    id INTEGER PRIMARY KEY,
    signature TEXT NOT NULL,
    agent TEXT NOT NULL,
    code TEXT NOT NULL,
    route TEXT NOT NULL,
    template TEXT NOT NULL,
    at INTEGER NOT NULL
) STRICT;
CREATE INDEX api_errors_signature ON api_errors (signature, at);
CREATE INDEX api_errors_at ON api_errors (at);
