-- The newest change an agent made (`Handoff.last_change_at`) is found by walking that
-- agent's events from the newest.
CREATE INDEX events_actor ON events (actor, seq);
