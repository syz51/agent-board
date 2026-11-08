-- How many tasks a board's coordinator and its child threads may have in progress at once
-- (docs/DESIGN.md, "Autopilot and the coordinator"). 1 keeps one task at a time.
ALTER TABLE boards ADD COLUMN parallel_tasks INTEGER NOT NULL DEFAULT 1;
