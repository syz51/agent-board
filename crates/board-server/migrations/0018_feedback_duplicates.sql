-- Items resolved as duplicates of an item, by that item: its reports, reporters and last
-- report count theirs when it is read.
CREATE INDEX feedback_duplicates ON feedback (resolution_task) WHERE resolution = 'duplicate';
