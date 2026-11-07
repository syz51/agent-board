-- Temporary boards are closed once idle and deleted later by the server's sweep; the user
-- may also delete a closed board.
ALTER TABLE boards ADD COLUMN temporary INTEGER NOT NULL DEFAULT 0;

-- Deleting a board deletes its asks. Ask ids live on outside the database (Telegram
-- buttons, web links), so they must never be handed out again: AUTOINCREMENT. Nothing
-- refers to asks, so the table is rebuilt in place.
CREATE TABLE asks_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    board TEXT NOT NULL REFERENCES boards (slug),
    task TEXT,
    agent TEXT NOT NULL,
    kind TEXT NOT NULL,
    question TEXT NOT NULL,
    options TEXT NOT NULL,
    status TEXT NOT NULL,
    answer TEXT,
    answered_via TEXT,
    created_at INTEGER NOT NULL,
    answered_at INTEGER,
    telegram_message_id INTEGER,
    telegram_chat_id INTEGER
) STRICT;
INSERT INTO asks_new (id, board, task, agent, kind, question, options, status, answer,
    answered_via, created_at, answered_at, telegram_message_id, telegram_chat_id)
SELECT id, board, task, agent, kind, question, options, status, answer, answered_via,
    created_at, answered_at, telegram_message_id, telegram_chat_id
FROM asks;
DROP TABLE asks;
ALTER TABLE asks_new RENAME TO asks;
CREATE INDEX asks_status ON asks (status, board);

-- A deleted board's slug keeps its task numbering, so a board created later under the
-- same slug never reuses the id of a deleted task that an agent may still hold.
CREATE TABLE deleted_boards (
    slug TEXT PRIMARY KEY,
    next_number INTEGER NOT NULL
) STRICT;
