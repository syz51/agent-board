-- Telegram pairing: `board admin telegram-pair` stores a one-time code (hashed); the first
-- private chat that sends the bot `/start <code>` before it expires becomes the chat the
-- notifier uses, unless server.toml names one.

CREATE TABLE telegram_pairing_codes (
    code_hash TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT;

-- At most one row.
CREATE TABLE telegram_chat (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    chat_id INTEGER NOT NULL,
    paired_at INTEGER NOT NULL
) STRICT;
