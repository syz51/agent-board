-- Track when the server first observed or last saw a run_count change on a scheduled job,
-- so silent exit-0 launchd StartInterval jobs do not falsely report overdue when stdout/stderr
-- log mtime lags behind launchctl print's runs counter (agent-board-763).

ALTER TABLE scheduled_jobs ADD COLUMN run_count_observed_at INTEGER;
