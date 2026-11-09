-- When a paused board was paused (docs/DESIGN.md, "Pausing work"): the hooks halt only a
-- turn that began before it. NULL while the board is not paused.
ALTER TABLE boards ADD COLUMN paused_at INTEGER;

-- Boards paused now: the time of their last status change, which paused them (events from
-- before payloads named the status list it in `fields`), else their last update.
UPDATE boards SET paused_at = COALESCE(
    (SELECT MAX(e.at) FROM events e
     WHERE e.board = boards.slug AND e.type = 'board_updated'
       AND EXISTS (SELECT 1 FROM json_each(e.payload, '$.fields') f WHERE f.value = 'status')),
    updated_at)
WHERE status = 'paused';
