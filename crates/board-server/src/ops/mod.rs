//! The board's rules, one module per area. Every function runs inside one transaction
//! (`Ctx`); requests are validated at the API boundary before they get here.

pub(crate) mod agents;
pub(crate) mod asks;
pub(crate) mod boards;
pub(crate) mod devices;
pub(crate) mod feedback;
pub(crate) mod leases;
pub(crate) mod stats;
pub(crate) mod tasks;
pub(crate) mod telegram;
pub(crate) mod updates;

use board_proto::{AgentId, BoardSlug, EventType};
use rusqlite::params;

use crate::db::Ctx;
use crate::error::Result;
use crate::store::agent_missing;

/// Checks that `agent` is registered and records that it was seen now.
pub(crate) fn touch_agent(cx: &mut Ctx<'_>, agent: &AgentId) -> Result<()> {
    let updated = cx.db.execute(
        "UPDATE agents SET last_seen = ?1 WHERE id = ?2",
        params![cx.now_ms(), agent.as_str()],
    )?;
    if updated == 0 {
        return Err(agent_missing(agent));
    }
    Ok(())
}

/// Adds `agent` to `board` unless it is a member; records `board_joined` when it joins.
pub(crate) fn join_board(cx: &mut Ctx<'_>, board: &BoardSlug, agent: &AgentId) -> Result<bool> {
    let added = cx.db.execute(
        "INSERT OR IGNORE INTO board_members (board, agent, joined_at) VALUES (?1, ?2, ?3)",
        params![board.as_str(), agent.as_str(), cx.now_ms()],
    )?;
    if added > 0 {
        cx.emit(
            Some(board),
            agent.as_str(),
            EventType::BoardJoined,
            None,
            serde_json::json!({ "agent": agent }),
        )?;
    }
    Ok(added > 0)
}

/// `ttl` in milliseconds from now.
pub(crate) fn expires(cx: &Ctx<'_>, ttl_ms: i64) -> i64 {
    cx.now_ms().saturating_add(ttl_ms)
}
