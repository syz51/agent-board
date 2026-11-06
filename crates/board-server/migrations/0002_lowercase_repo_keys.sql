-- Repo keys are now lowercase as a whole (`board_proto::normalize_repo`); before, only the
-- host was. Stored keys were already normalized apart from case, so lowercasing them gives
-- the new key. SQLite's lower() folds ASCII only, as normalize_repo does.
--
-- Not recoverable here: an scp URL with an all-digit owner (`git@host:12345/repo`) used to
-- lose the owner (`host/repo`). Agents refresh their repo when they next register, leases
-- and touches expire within the hour, and a board holding such a key needs its repos set
-- again. Event payloads are history and stay as written.

-- A board's repos keep their order; keys that now coincide appear once.
UPDATE boards SET repos = (
    SELECT json_group_array(repo) FROM (
        SELECT lower(value) AS repo FROM json_each(boards.repos)
        GROUP BY lower(value) ORDER BY MIN(json_each.key)
    )
) WHERE repos != lower(repos);

UPDATE agents SET repo = lower(repo) WHERE repo != lower(repo);

UPDATE leases SET repo = lower(repo) WHERE repo != lower(repo);

-- (agent, repo, path) is unique: keep the latest touch of keys that now coincide.
DELETE FROM touches WHERE EXISTS (
    SELECT 1 FROM touches t
    WHERE t.agent = touches.agent AND t.path = touches.path
      AND lower(t.repo) = lower(touches.repo) AND t.repo != touches.repo
      AND (t.at > touches.at OR (t.at = touches.at AND t.repo < touches.repo))
);
UPDATE touches SET repo = lower(repo) WHERE repo != lower(repo);
