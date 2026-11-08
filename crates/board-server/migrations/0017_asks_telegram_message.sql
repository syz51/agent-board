-- A reply from Telegram finds its ask by the chat and the message it replies to; message
-- ids are unique only per chat.
CREATE INDEX asks_telegram_message ON asks (telegram_chat_id, telegram_message_id);
