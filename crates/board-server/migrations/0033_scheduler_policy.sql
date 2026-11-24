-- Slice 4.4 (docs/SCHEDULER_DESIGN.md): versioned declarative scheduler policies,
-- compact rolling scheduler hold telemetry, and deduplicated pathology detection state.

-- ---------------------------------------------------------------------------
-- Versioned declarative scheduler policies
-- ---------------------------------------------------------------------------
CREATE TABLE scheduler_policies (
    version TEXT PRIMARY KEY,
    weights_json TEXT NOT NULL,
    lease_ttl_secs INTEGER NOT NULL,
    sticky_grace_secs INTEGER NOT NULL,
    notes TEXT,
    active INTEGER NOT NULL DEFAULT 0,
    created_by TEXT,
    created_at INTEGER NOT NULL
) STRICT;

CREATE UNIQUE INDEX scheduler_policies_single_active
    ON scheduler_policies (active) WHERE active = 1;
CREATE INDEX scheduler_policies_created
    ON scheduler_policies (created_at DESC, version DESC);

INSERT INTO scheduler_policies (
    version, weights_json, lease_ttl_secs, sticky_grace_secs, notes, active, created_by, created_at
) VALUES (
    'v1-default',
    '{"w_kind":100.0,"w_prio":30.0,"w_rank":10.0,"w_age":16.0,"w_fair":12.0,"w_loc":8.0,"w_pref":1.0,"w_backfill":6.0}',
    180,
    600,
    'Built-in default multifactor scheduler policy',
    1,
    'server',
    0
);

-- ---------------------------------------------------------------------------
-- Compact rolling scheduler hold telemetry (pruned by retention sweeper)
-- ---------------------------------------------------------------------------
CREATE TABLE scheduler_holds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    board TEXT,
    task_id TEXT,
    device TEXT,
    reason TEXT NOT NULL,
    detail TEXT,
    conflict_key TEXT
) STRICT;

CREATE INDEX scheduler_holds_at ON scheduler_holds (at);
CREATE INDEX scheduler_holds_board_at ON scheduler_holds (board, at);
CREATE INDEX scheduler_holds_dedup ON scheduler_holds (board, task_id, reason, at);
CREATE INDEX scheduler_holds_conflict_at ON scheduler_holds (conflict_key, at)
    WHERE conflict_key IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Deduplicated pathology detection state (prevents 60s sweeper alert storms)
-- ---------------------------------------------------------------------------
CREATE TABLE pathology_state (
    signature TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    last_filed_at INTEGER
) STRICT;

CREATE INDEX pathology_state_kind ON pathology_state (kind);

-- Supporting indexes on run_outcomes for GET /v1/stats/scheduler breakdowns.
CREATE INDEX run_outcomes_policy_recorded ON run_outcomes (policy_version, recorded_at);
CREATE INDEX run_outcomes_device_recorded ON run_outcomes (device, recorded_at);
