-- Telegram message ids are per chat, so an ask records the chat its message went to; a
-- withdrawn ask's message is edited there, whichever chat is current.
ALTER TABLE asks ADD COLUMN telegram_chat_id INTEGER;
