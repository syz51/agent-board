-- The board change an ask carries (docs/API.md, "Approved board changes"): JSON of the
-- settings the server applies when the user answers with the ask's approving option.
ALTER TABLE asks ADD COLUMN change TEXT;
