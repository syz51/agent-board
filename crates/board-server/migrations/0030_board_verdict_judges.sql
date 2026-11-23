-- Whether a home device daemon starts automatic verdict judges for pending proposals on
-- a board that is not on autopilot (docs/DESIGN.md, "Daemon dispatch", Boards off
-- autopilot). 1 keeps the default behavior from agent-board-491.
ALTER TABLE boards ADD COLUMN verdict_judges INTEGER NOT NULL DEFAULT 1;
