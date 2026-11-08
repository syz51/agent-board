-- The newest report an item had when it was resolved (`feedback_reports.id`, 0 for none), so
-- reports after a decline are told from earlier ones exactly, past the capped `reporters`.
-- Items resolved before this column take the reports recorded by their resolution time.
ALTER TABLE feedback ADD COLUMN resolved_report INTEGER;
UPDATE feedback SET resolved_report = (
    SELECT COALESCE(MAX(r.id), 0) FROM feedback_reports r
    WHERE r.task = feedback.task AND r.at <= feedback.resolved_at
) WHERE resolved_at IS NOT NULL;
