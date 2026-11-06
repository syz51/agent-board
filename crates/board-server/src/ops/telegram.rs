//! Telegram pairing: one-time codes created on the host, redeemed by `/start <code>` from
//! a private chat, which then becomes the notifier's chat.

use rusqlite::{OptionalExtension, params};

use crate::auth::hash_token;
use crate::db::Ctx;
use crate::error::{Result, invalid};

/// How long a pairing code stays valid.
pub(crate) const CODE_TTL_MS: i64 = 15 * 60 * 1000;
/// Letters and digits that cannot be misread for one another.
const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// 12 characters from 31: about 59 bits.
const CODE_LEN: usize = 12;

/// The paired chat, if any.
pub(crate) fn paired_chat(cx: &Ctx<'_>) -> Result<Option<i64>> {
    Ok(cx
        .db
        .query_row("SELECT chat_id FROM telegram_chat WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Creates a pairing code and returns it; only its hash is stored. Refused while a chat is
/// paired, unless `replace`, which unpairs it first.
pub(crate) fn create_code(cx: &mut Ctx<'_>, replace: bool) -> Result<String> {
    if paired_chat(cx)?.is_some() {
        if !replace {
            return Err(invalid(
                "a Telegram chat is already paired",
                "pass --replace to unpair it and pair another chat",
            ));
        }
        cx.db.execute("DELETE FROM telegram_chat", [])?;
    }
    cx.db.execute(
        "DELETE FROM telegram_pairing_codes WHERE expires_at <= ?1",
        [cx.now_ms()],
    )?;
    let code = new_code();
    cx.db.execute(
        "INSERT INTO telegram_pairing_codes (code_hash, created_at, expires_at) \
         VALUES (?1, ?2, ?3)",
        params![hash_token(&code), cx.now_ms(), cx.now_ms() + CODE_TTL_MS],
    )?;
    Ok(code)
}

fn new_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    rand::fill(&mut bytes);
    // 256 is not a multiple of 31, so reject the top values to stay uniform.
    let mut code = String::with_capacity(CODE_LEN);
    let limit = 256 - 256 % ALPHABET.len();
    while code.len() < CODE_LEN {
        for &b in &bytes {
            if (b as usize) < limit && code.len() < CODE_LEN {
                code.push(ALPHABET[b as usize % ALPHABET.len()] as char);
            }
        }
        rand::fill(&mut bytes);
    }
    code
}

/// Pairs `chat_id` when `code` is a live pairing code and no chat is paired; every code is
/// used up by a successful pairing. Returns whether the chat was paired.
pub(crate) fn redeem(cx: &mut Ctx<'_>, code: &str, chat_id: i64) -> Result<bool> {
    if paired_chat(cx)?.is_some() {
        return Ok(false);
    }
    let code = code.trim().to_ascii_lowercase();
    let live: Option<i64> = cx
        .db
        .query_row(
            "SELECT expires_at FROM telegram_pairing_codes \
             WHERE code_hash = ?1 AND expires_at > ?2",
            params![hash_token(&code), cx.now_ms()],
            |r| r.get(0),
        )
        .optional()?;
    if live.is_none() {
        return Ok(false);
    }
    cx.db.execute("DELETE FROM telegram_pairing_codes", [])?;
    cx.db.execute(
        "INSERT INTO telegram_chat (id, chat_id, paired_at) VALUES (1, ?1, ?2)",
        params![chat_id, cx.now_ms()],
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_use_the_alphabet() {
        for _ in 0..50 {
            let code = new_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
        }
        assert_ne!(new_code(), new_code());
    }
}
