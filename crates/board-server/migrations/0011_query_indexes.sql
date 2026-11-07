-- Indexes for lookups that scanned a whole table.
-- A task's children: deleting a task checks the parent foreign key, and deleting a board
-- deletes its tasks one by one.
CREATE INDEX tasks_parent ON tasks (parent);
-- The threads an agent delegated (delegation_family in ops/leases.rs). Queries must use
-- this exact expression for the index to apply.
CREATE INDEX agents_t3_parent ON agents (json_extract(native_ids, '$.t3_parent'));
