-- The account usage a device daemon last read (docs/DESIGN.md, "Usage"). One row: the
-- newest reading by `read_at`, as JSON in the shape of board_proto::Usage.
CREATE TABLE usage (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    reading TEXT NOT NULL,
    read_at INTEGER NOT NULL
) STRICT;

-- The usage percent at which a board starts no new task (docs/DESIGN.md, "Usage").
ALTER TABLE boards ADD COLUMN usage_stop_at INTEGER NOT NULL DEFAULT 90;
