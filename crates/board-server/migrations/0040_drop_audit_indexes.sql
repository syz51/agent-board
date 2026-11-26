-- Indexes for dropped-task and verdict-cut audits in /v1/updates and pathology sweeps.
CREATE INDEX IF NOT EXISTS tasks_status_updated ON tasks(status, updated_at DESC, number DESC);
CREATE INDEX IF NOT EXISTS notes_kind_at ON notes(kind, at DESC, id DESC);
