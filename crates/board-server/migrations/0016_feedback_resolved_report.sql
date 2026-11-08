-- The newest report of any item when an item was resolved (`feedback_reports.id`, 0 for
-- none), so reports after a decline are told from earlier ones by insertion order, past the
-- capped `reporters`. Items resolved before this column take the reports recorded by their
-- resolution time.
ALTER TABLE feedback ADD COLUMN resolved_report INTEGER;
UPDATE feedback SET resolved_report = (
    SELECT COALESCE(MAX(r.id), 0) FROM feedback_reports r WHERE r.at <= feedback.resolved_at
) WHERE resolved_at IS NOT NULL;
