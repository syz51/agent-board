-- How a touch was found and the file's modification time the client saw. A path found by
-- scanning the work tree after a shell call (`scanned = 1`) whose time equals a peer's
-- edit-tool touch of it was the peer's write, not the scanning agent's.
ALTER TABLE touches ADD COLUMN scanned INTEGER NOT NULL DEFAULT 0;
ALTER TABLE touches ADD COLUMN modified INTEGER;
