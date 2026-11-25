-- Per-board goal-review interval (agent-board-604) and token usage columns in run_outcomes (agent-board-726).

ALTER TABLE boards ADD COLUMN goal_review_every_secs INTEGER;
UPDATE boards SET goal_review_every_secs = 21600 WHERE slug = 'board-feedback' AND goal_review_every_secs IS NULL;

ALTER TABLE run_outcomes ADD COLUMN cached_tokens INTEGER;
ALTER TABLE run_outcomes ADD COLUMN reasoning_tokens INTEGER;
