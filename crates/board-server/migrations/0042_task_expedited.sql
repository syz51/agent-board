-- User-only expedite flag bypassing soft daily usage pace holds (agent-board-824).
ALTER TABLE tasks ADD COLUMN expedited INTEGER NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS tasks_expedited ON tasks (board, expedited) WHERE expedited = 1;
