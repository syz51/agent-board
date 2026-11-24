-- Automated scheduler evaluation and guardrailed self-tuning
-- (`docs/SCHEDULER_DESIGN.md`, Section 6.4).

-- Track auto-promoted policy provenance and rollback targets on `scheduler_policies`.
ALTER TABLE scheduler_policies ADD COLUMN auto_promoted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE scheduler_policies ADD COLUMN previous_version TEXT;

-- Audit log of every automated scheduler evaluation and self-tuning decision.
CREATE TABLE scheduler_evaluations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    evaluated_at INTEGER NOT NULL,
    mode TEXT NOT NULL,
    decision TEXT NOT NULL,
    baseline_policy_version TEXT NOT NULL,
    candidate_policy_version TEXT,
    applied_policy_version TEXT,
    previous_policy_version TEXT,
    window_days INTEGER NOT NULL,
    sample_runs INTEGER NOT NULL,
    sample_tasks INTEGER NOT NULL,
    baseline_metrics_json TEXT,
    candidate_metrics_json TEXT,
    candidate_weights_json TEXT,
    score_delta_pct REAL,
    reason TEXT NOT NULL,
    feedback_task_id TEXT
) STRICT;

CREATE INDEX scheduler_evaluations_at
    ON scheduler_evaluations (evaluated_at DESC, id DESC);
