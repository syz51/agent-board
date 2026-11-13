-- A task's place inside its priority level (docs/DESIGN.md, "Order inside a priority level"):
-- lower first, and a task without one after every ranked task, by number.
ALTER TABLE tasks ADD COLUMN rank INTEGER;
