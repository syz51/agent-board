-- Mark intermittent/laptop hosts that legitimately sleep offline so health posture and
-- scheduled jobs treat them as asleep rather than degraded/overdue.
ALTER TABLE devices ADD COLUMN sleepable INTEGER NOT NULL DEFAULT 0;
