-- The weekly usage pace (docs/DESIGN.md, "Usage", Pace). One row: the reserve (NULL: the
-- pace is off), the current pace day's start and its first weekly reading, and since when
-- the measured weekly use is at the hard line.
CREATE TABLE usage_pace (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    reserve REAL,
    day_start INTEGER,
    anchor REAL,
    hard_since INTEGER
) STRICT;
INSERT INTO usage_pace (id) VALUES (1);

-- The last hour of weekly readings, for the burn rate.
CREATE TABLE usage_weekly (
    read_at INTEGER PRIMARY KEY,
    percent REAL NOT NULL
) STRICT;
