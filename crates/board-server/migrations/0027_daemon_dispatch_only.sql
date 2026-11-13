-- Every board is dispatched by the device daemon (docs/DESIGN.md, "Daemon dispatch"): the
-- coordinator-thread path is gone. A board that still ran a coordinator thread holds that
-- thread from now on, as a switch to daemon dispatch did.
INSERT INTO stood_down (board, agent, since, reason)
SELECT slug, last_coordinator, CAST(unixepoch('subsec') * 1000 AS INTEGER),
    'the board is set to daemon dispatch'
FROM boards
WHERE dispatch = 'thread' AND last_coordinator IS NOT NULL
ON CONFLICT (board, agent) DO NOTHING;

UPDATE boards SET dispatch = 'daemon' WHERE dispatch = 'thread';
