-- Bounded host health snapshots, 30-day hourly resource samples, normalized
-- cross-machine scheduled jobs, and ring-buffered job execution history (agent-board-756).

CREATE TABLE host_health_snapshots (
    host TEXT NOT NULL,
    collector TEXT NOT NULL,
    report_json TEXT NOT NULL,
    summary_hash TEXT NOT NULL,
    reported_at INTEGER NOT NULL,
    ttl_secs INTEGER NOT NULL DEFAULT 300,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (host, collector)
) STRICT;

CREATE INDEX host_health_snapshots_host ON host_health_snapshots (host);

CREATE TABLE host_health_samples (
    host TEXT NOT NULL,
    hour_bucket INTEGER NOT NULL,
    cpu_pct REAL,
    mem_used_pct REAL,
    disk_used_pct REAL,
    disk_free_gib REAL,
    samples INTEGER NOT NULL DEFAULT 1,
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (host, hour_bucket)
) STRICT;

CREATE INDEX host_health_samples_recorded ON host_health_samples (recorded_at);

CREATE TABLE scheduled_jobs (
    job_key TEXT PRIMARY KEY,
    host TEXT NOT NULL,
    source TEXT NOT NULL,
    job_id TEXT NOT NULL,
    name TEXT NOT NULL,
    category TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    schedule_human TEXT NOT NULL,
    schedule_raw TEXT NOT NULL,
    expected_interval_secs INTEGER,
    state TEXT NOT NULL,
    last_run_at INTEGER,
    next_run_at INTEGER,
    last_duration_ms INTEGER,
    last_exit_code INTEGER,
    run_count INTEGER,
    last_error TEXT,
    command_preview TEXT,
    deep_link TEXT,
    reported_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX scheduled_jobs_host_source ON scheduled_jobs (host, source);
CREATE INDEX scheduled_jobs_state ON scheduled_jobs (state);
CREATE INDEX scheduled_jobs_category ON scheduled_jobs (category);

CREATE TABLE scheduled_job_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_key TEXT NOT NULL REFERENCES scheduled_jobs(job_key) ON DELETE CASCADE,
    at INTEGER NOT NULL,
    state TEXT NOT NULL,
    exit_code INTEGER,
    duration_ms INTEGER,
    error_summary TEXT,
    run_count INTEGER
) STRICT;

CREATE INDEX scheduled_job_history_job_at ON scheduled_job_history (job_key, at DESC, id DESC);
