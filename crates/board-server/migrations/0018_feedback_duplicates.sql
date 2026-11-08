-- Items resolved as duplicates of an item, by that item: its reports, reporters and last
-- report count theirs when it is read.
CREATE INDEX feedback_duplicates ON feedback (resolution_task) WHERE resolution = 'duplicate';

-- Whether a report is an API error the server counted (1) or an agent's report (0), so an
-- item whose duplicates mix both tells them apart. Rows recorded before this column are
-- classed by their item's source.
ALTER TABLE feedback_reports ADD COLUMN signal INTEGER NOT NULL DEFAULT 0;
UPDATE feedback_reports SET signal = 1
WHERE task IN (SELECT task FROM feedback WHERE source = 'signal');
