-- Timed raises/overrides of board limits and usage reserve (`docs/API.md`).
ALTER TABLE boards ADD COLUMN parallel_tasks_prev INTEGER;
ALTER TABLE boards ADD COLUMN parallel_tasks_until INTEGER;
ALTER TABLE boards ADD COLUMN usage_stop_at_prev INTEGER;
ALTER TABLE boards ADD COLUMN usage_stop_at_until INTEGER;

ALTER TABLE usage_buckets ADD COLUMN reserve_prev_pct REAL;
ALTER TABLE usage_buckets ADD COLUMN reserve_until INTEGER;
