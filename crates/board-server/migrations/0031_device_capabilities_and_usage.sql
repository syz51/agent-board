-- Slice 1.1 (docs/SCHEDULER_DESIGN.md): device capabilities & plane metadata,
-- optional task routing columns, and multi-provider usage buckets.

-- ---------------------------------------------------------------------------
-- Devices: admin-controlled plane metadata + device-self heartbeat metadata
-- ---------------------------------------------------------------------------
ALTER TABLE devices ADD COLUMN role TEXT NOT NULL DEFAULT 'runner';
ALTER TABLE devices ADD COLUMN os TEXT;
ALTER TABLE devices ADD COLUMN arch TEXT;
ALTER TABLE devices ADD COLUMN capacity_json TEXT;
ALTER TABLE devices ADD COLUMN free_units INTEGER;
ALTER TABLE devices ADD COLUMN capabilities_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE devices ADD COLUMN runners_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE devices ADD COLUMN repos_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE devices ADD COLUMN taints_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE devices ADD COLUMN preference INTEGER NOT NULL DEFAULT 0;
ALTER TABLE devices ADD COLUMN protocol_features_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE devices ADD COLUMN drain INTEGER NOT NULL DEFAULT 0;
ALTER TABLE devices ADD COLUMN heartbeat_at INTEGER;

-- ---------------------------------------------------------------------------
-- Tasks: optional machine-scheduling & routing metadata
-- ---------------------------------------------------------------------------
ALTER TABLE tasks ADD COLUMN repo TEXT;
ALTER TABLE tasks ADD COLUMN weight TEXT;
ALTER TABLE tasks ADD COLUMN resource_units INTEGER;
ALTER TABLE tasks ADD COLUMN requires_json TEXT;
ALTER TABLE tasks ADD COLUMN tolerations_json TEXT;
ALTER TABLE tasks ADD COLUMN conflict_keys_json TEXT;
ALTER TABLE tasks ADD COLUMN affinity_json TEXT;
ALTER TABLE tasks ADD COLUMN preferred_runner TEXT;

-- ---------------------------------------------------------------------------
-- Usage buckets: per-(provider, account) quota windows, pace & weekly samples
-- ---------------------------------------------------------------------------
CREATE TABLE usage_buckets (
    provider TEXT NOT NULL,
    account TEXT NOT NULL,
    five_hour_pct REAL,
    five_hour_resets_at INTEGER,
    seven_day_pct REAL,
    seven_day_resets_at INTEGER,
    read_at INTEGER,
    device TEXT,
    reserve_pct REAL,
    anchor_day_start INTEGER,
    anchor_pct REAL,
    hard_since INTEGER,
    PRIMARY KEY (provider, account)
) STRICT;

CREATE TABLE usage_bucket_samples (
    provider TEXT NOT NULL,
    account TEXT NOT NULL,
    at INTEGER NOT NULL,
    seven_day_pct REAL NOT NULL,
    PRIMARY KEY (provider, account, at)
) STRICT;

-- Backfill any existing legacy usage / usage_pace / usage_weekly data into
-- ('anthropic', 'default') so upgrades lose zero data.
INSERT OR REPLACE INTO usage_buckets (
    provider,
    account,
    five_hour_pct,
    five_hour_resets_at,
    seven_day_pct,
    seven_day_resets_at,
    read_at,
    device,
    reserve_pct,
    anchor_day_start,
    anchor_pct,
    hard_since
)
SELECT
    'anthropic',
    'default',
    CAST(json_extract(u.reading, '$.five_hour.percent') AS REAL),
    CASE
        WHEN json_extract(u.reading, '$.five_hour.resets_at') IS NOT NULL
        THEN CAST(ROUND(unixepoch(json_extract(u.reading, '$.five_hour.resets_at'), 'subsec') * 1000.0) AS INTEGER)
        ELSE NULL
    END,
    CAST(json_extract(u.reading, '$.seven_day.percent') AS REAL),
    CASE
        WHEN json_extract(u.reading, '$.seven_day.resets_at') IS NOT NULL
        THEN CAST(ROUND(unixepoch(json_extract(u.reading, '$.seven_day.resets_at'), 'subsec') * 1000.0) AS INTEGER)
        ELSE NULL
    END,
    u.read_at,
    json_extract(u.reading, '$.device'),
    p.reserve,
    p.day_start,
    p.anchor,
    p.hard_since
FROM usage_pace p
LEFT JOIN usage u ON u.id = 1
WHERE p.id = 1
  AND (
      u.id IS NOT NULL
      OR p.reserve IS NOT NULL
      OR p.day_start IS NOT NULL
      OR p.anchor IS NOT NULL
      OR p.hard_since IS NOT NULL
  );

INSERT OR IGNORE INTO usage_bucket_samples (provider, account, at, seven_day_pct)
SELECT 'anthropic', 'default', read_at, percent
FROM usage_weekly;
