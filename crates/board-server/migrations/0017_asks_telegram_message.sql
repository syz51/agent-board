-- A reply from Telegram finds its ask by the chat and the message it replies to; message
-- ids are unique only per chat. Only asks sent to Telegram are indexed.
CREATE INDEX asks_telegram_message ON asks (telegram_chat_id, telegram_message_id)
    WHERE telegram_message_id IS NOT NULL;
-- Asks sent before 0005 recorded no chat. With a paired chat they were sent to it, since
-- pairing another chat forgets every message id. A configured chat is not stored, so
-- replies to such asks sent there no longer match.
UPDATE asks SET telegram_chat_id = (SELECT chat_id FROM telegram_chat WHERE id = 1)
WHERE telegram_chat_id IS NULL AND telegram_message_id IS NOT NULL;
