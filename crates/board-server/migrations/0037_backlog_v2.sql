-- Backlog v2 schema (agent-board-740): task origin, size, Icebox cooled_at,
-- ready_at, board kind (project vs inbox), and curate_every_secs.

ALTER TABLE tasks ADD COLUMN origin TEXT NOT NULL DEFAULT 'agent';
ALTER TABLE tasks ADD COLUMN size TEXT;
ALTER TABLE tasks ADD COLUMN cooled_at INTEGER;
ALTER TABLE tasks ADD COLUMN ready_at INTEGER;

UPDATE tasks SET ready_at = created_at WHERE status = 'open' AND ready_at IS NULL;

-- Backfill origin = 'feedback' for existing feedback items.
UPDATE tasks SET origin = 'feedback'
WHERE id IN (SELECT task FROM feedback) AND origin = 'agent';

CREATE INDEX IF NOT EXISTS tasks_cooled_at ON tasks (board, cooled_at) WHERE cooled_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS feedback_routed_task ON feedback (resolution_task) WHERE resolution = 'routed' AND resolution_task IS NOT NULL;
CREATE INDEX IF NOT EXISTS feedback_recurs ON feedback (recurs) WHERE recurs IS NOT NULL;
CREATE INDEX IF NOT EXISTS task_blockers_blocker ON task_blockers (blocker);

ALTER TABLE boards ADD COLUMN kind TEXT NOT NULL DEFAULT 'project';
ALTER TABLE boards ADD COLUMN curate_every_secs INTEGER;

UPDATE boards
SET kind = 'inbox', goal_review_every_secs = 0
WHERE slug = 'board-feedback';
