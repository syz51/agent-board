-- Daemon dispatch (docs/DESIGN.md, "Daemon dispatch"). `dispatch` says who runs a board's
-- autopilot: a coordinator thread (`thread`) or the daemon holding the coordinator lease
-- (`daemon`).
ALTER TABLE boards ADD COLUMN dispatch TEXT NOT NULL DEFAULT 'thread';

-- Judges the coordinator lease holder registered for its board. A judge has the
-- coordinator's rights on the board until `ended_at` (its agent ended) or `expires_at`.
-- One row per board and agent; a registration that is over is replaced by a new one.
CREATE TABLE judges (
    board TEXT NOT NULL REFERENCES boards (slug),
    agent TEXT NOT NULL,
    kind TEXT NOT NULL,
    tasks TEXT NOT NULL,
    registered_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    ended_at INTEGER,
    PRIMARY KEY (board, agent)
) STRICT;
CREATE INDEX judges_agent ON judges (agent);
