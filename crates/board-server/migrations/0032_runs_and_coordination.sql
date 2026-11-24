-- Slice 1.2 (docs/SCHEDULER_DESIGN.md): unified runs, bounded run event tail,
-- quantitative run outcomes, fleet-wide conflict locks, and sticky wakes.

-- ---------------------------------------------------------------------------
-- Runs: first-class ephemeral execution attempts (managed and interactive)
-- ---------------------------------------------------------------------------
CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    board TEXT REFERENCES boards(slug) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    origin TEXT NOT NULL,
    device TEXT NOT NULL REFERENCES devices(name),
    runner TEXT NOT NULL,
    agent_id TEXT,
    state TEXT NOT NULL,
    lease_expires_at INTEGER,
    worktree_branch TEXT,
    cwd TEXT,
    title TEXT,
    attach_hint TEXT,
    cost_units INTEGER NOT NULL DEFAULT 1,
    policy_version TEXT,
    outcome_json TEXT,
    transcript_uri TEXT,
    transcript_sha256 TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    ended_at INTEGER,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX runs_board_state ON runs (board, state);
CREATE INDEX runs_task_kind_state ON runs (task_id, kind, state);
CREATE INDEX runs_device_state ON runs (device, state);
CREATE INDEX runs_agent_state ON runs (agent_id, state);
CREATE INDEX runs_lease_expires ON runs (lease_expires_at) WHERE lease_expires_at IS NOT NULL;
CREATE INDEX runs_created ON runs (created_at DESC, id DESC);

-- ---------------------------------------------------------------------------
-- Run events: bounded live event tail (ready for Slice 4.2)
-- ---------------------------------------------------------------------------
CREATE TABLE run_events (
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    summary TEXT NOT NULL,
    meta_json TEXT,
    PRIMARY KEY (run_id, seq)
) STRICT;

-- ---------------------------------------------------------------------------
-- Run outcomes: quantitative run & scheduler telemetry (ready for Slice 4.4)
-- ---------------------------------------------------------------------------
CREATE TABLE run_outcomes (
    run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
    board TEXT,
    task_id TEXT,
    device TEXT,
    kind TEXT NOT NULL,
    terminal_state TEXT NOT NULL,
    ready_to_leased_secs INTEGER,
    leased_to_running_secs INTEGER,
    running_secs INTEGER,
    conflict_wait_secs INTEGER,
    weight_unit_minutes REAL,
    quota_bucket TEXT,
    tokens_in INTEGER,
    tokens_out INTEGER,
    warm_cache_hit INTEGER,
    orphan_count INTEGER NOT NULL DEFAULT 0,
    retry_count INTEGER NOT NULL DEFAULT 0,
    ci_runs_before_merge INTEGER,
    review_rounds INTEGER,
    first_pass_merge INTEGER,
    policy_version TEXT,
    recorded_at INTEGER NOT NULL
) STRICT;

CREATE INDEX run_outcomes_board_recorded ON run_outcomes (board, recorded_at);
CREATE INDEX run_outcomes_recorded ON run_outcomes (recorded_at);

-- ---------------------------------------------------------------------------
-- Conflict locks: fleet-wide concurrency semaphores held by active runs
-- ---------------------------------------------------------------------------
CREATE TABLE conflict_locks (
    key TEXT NOT NULL,
    run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    max_holders INTEGER NOT NULL,
    acquired_at INTEGER NOT NULL,
    PRIMARY KEY (key, run_id)
) STRICT;

CREATE INDEX conflict_locks_run_id ON conflict_locks (run_id);

-- Partial index for legacy judge-registration lookups in standing review verification.
CREATE INDEX events_board_judge ON events (board, json_extract(payload, '$.judge'))
    WHERE type = 'board_updated' AND json_extract(payload, '$.judge') IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Sticky wakes: per-device wake-up queue for active sessions
-- ---------------------------------------------------------------------------
CREATE TABLE sticky_wakes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT REFERENCES runs(id) ON DELETE CASCADE,
    agent_id TEXT,
    device TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    delivered_at INTEGER,
    expires_at INTEGER
) STRICT;

CREATE INDEX sticky_wakes_device_pending ON sticky_wakes (device, delivered_at, created_at);
CREATE INDEX sticky_wakes_run ON sticky_wakes (run_id) WHERE run_id IS NOT NULL;
CREATE INDEX sticky_wakes_agent ON sticky_wakes (agent_id) WHERE agent_id IS NOT NULL;
