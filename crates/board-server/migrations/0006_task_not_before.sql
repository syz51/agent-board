-- A task is not ready before `not_before` (ms since the epoch). Once it passes, the sweeper
-- sets `not_before_announced` and records a `task_updated` event that wakes the
-- coordinator; setting a new `not_before` resets it.
ALTER TABLE tasks ADD COLUMN not_before INTEGER;
ALTER TABLE tasks ADD COLUMN not_before_announced INTEGER NOT NULL DEFAULT 0;
CREATE INDEX tasks_not_before_due ON tasks (not_before)
    WHERE not_before IS NOT NULL AND not_before_announced = 0;
