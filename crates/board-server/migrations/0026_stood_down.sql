-- Stood-down coordinators (docs/DESIGN.md, "A coordinator that stands down"). The thread a
-- board last named coordinator, kept after its lease is released or lapses, so the server
-- knows which thread to hold when the board stops running it.
ALTER TABLE boards ADD COLUMN last_coordinator TEXT;
ALTER TABLE boards ADD COLUMN last_coordinator_device TEXT;

-- A coordinator thread the board no longer runs: autopilot went off, the board went to
-- daemon dispatch or closed, its device stopped being a home, or another thread took the
-- board. The snapshot holds the agent (`held_by`), so its hooks end its turns that the user
-- did not start, whether or not the daemon's stand-down message reached it. A claim that
-- names the agent coordinator again deletes the row.
CREATE TABLE stood_down (
    board TEXT NOT NULL REFERENCES boards (slug),
    agent TEXT NOT NULL,
    since INTEGER NOT NULL,
    reason TEXT NOT NULL,
    PRIMARY KEY (board, agent)
) STRICT;
CREATE INDEX stood_down_agent ON stood_down (agent);

-- The last coordinator of each board: the one named now, else the last one an event named.
-- Dispatch agents (`dispatch:<device>`) are the daemon, not a thread, and are not kept.
UPDATE boards SET
    last_coordinator = COALESCE(
        CASE WHEN coordinator NOT LIKE 'dispatch:%' THEN coordinator END,
        (SELECT json_extract(e.payload, '$.coordinator') FROM events e
         WHERE e.board = boards.slug AND e.type = 'board_updated'
           AND json_extract(e.payload, '$.coordinator') IS NOT NULL
           AND json_extract(e.payload, '$.coordinator') NOT LIKE 'dispatch:%'
         ORDER BY e.seq DESC LIMIT 1)),
    last_coordinator_device = COALESCE(
        CASE WHEN coordinator NOT LIKE 'dispatch:%' THEN coordinator_device END,
        (SELECT json_extract(e.payload, '$.device') FROM events e
         WHERE e.board = boards.slug AND e.type = 'board_updated'
           AND json_extract(e.payload, '$.coordinator') IS NOT NULL
           AND json_extract(e.payload, '$.coordinator') NOT LIKE 'dispatch:%'
         ORDER BY e.seq DESC LIMIT 1));

-- Boards that do not run a coordinator thread now hold the thread that last coordinated
-- them, from their last change of autopilot, dispatch or status.
INSERT INTO stood_down (board, agent, since, reason)
SELECT slug, last_coordinator,
    COALESCE(
        (SELECT MAX(e.at) FROM events e
         WHERE e.board = boards.slug AND e.type = 'board_updated'
           AND EXISTS (SELECT 1 FROM json_each(e.payload, '$.fields') f
                       WHERE f.value IN ('autopilot', 'dispatch', 'status'))),
        updated_at),
    CASE
        WHEN status = 'closed' THEN 'the board is closed'
        WHEN dispatch = 'daemon' THEN 'the board is set to daemon dispatch'
        ELSE 'autopilot was switched off'
    END
FROM boards
WHERE last_coordinator IS NOT NULL
  AND (autopilot = 0 OR dispatch = 'daemon' OR status = 'closed');
