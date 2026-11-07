-- A task is not ready before `not_before` (ms since the epoch); the sweeper clears it once
-- passed, which records a `task_updated` event that wakes the coordinator.
ALTER TABLE tasks ADD COLUMN not_before INTEGER;
CREATE INDEX tasks_not_before ON tasks (not_before) WHERE not_before IS NOT NULL;
