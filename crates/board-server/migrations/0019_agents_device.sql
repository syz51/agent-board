-- Agents by device, which touch() and shared_scan() search per path for the peers that can
-- share the caller's work tree. Agent rows are never deleted, so without it each lookup
-- scans every agent ever registered.
CREATE INDEX agents_device ON agents (device);
