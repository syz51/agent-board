//! The board's rules, one module per area. Every function runs inside one transaction
//! (`Ctx`); requests are validated at the API boundary before they get here.

pub(crate) mod agents;
pub(crate) mod asks;
pub(crate) mod boards;
pub(crate) mod devices;
pub(crate) mod feedback;
pub(crate) mod judges;
pub(crate) mod leases;
pub(crate) mod stats;
pub(crate) mod tasks;
pub(crate) mod telegram;
pub(crate) mod updates;
pub(crate) mod usage;

use board_proto::{AgentId, BoardSlug, EventType};
use rusqlite::{OptionalExtension, params};

use crate::db::Ctx;
use crate::error::Result;
use crate::store::agent_missing;

/// Checks that `agent` is registered and records that it was seen now. For calls the agent
/// makes as itself: one marked ended is working after all, so it is running again,
/// announced once. Calls made for it by the user use [`note_agent_seen`]; a device claiming
/// the coordinator lease for it marks neither.
pub(crate) fn touch_agent(cx: &mut Ctx<'_>, agent: &AgentId) -> Result<()> {
    let state: String = cx
        .db
        .query_row(
            "SELECT state FROM agents WHERE id = ?1",
            [agent.as_str()],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| agent_missing(agent))?;
    let revive = state == "ended";
    cx.db.execute(
        "UPDATE agents SET last_seen = ?1, \
         state = CASE WHEN ?3 THEN 'running' ELSE state END, \
         paused_until = CASE WHEN ?3 THEN NULL ELSE paused_until END \
         WHERE id = ?2",
        params![cx.now_ms(), agent.as_str(), revive],
    )?;
    if revive {
        cx.emit(
            None,
            agent.as_str(),
            EventType::AgentStateChanged,
            None,
            serde_json::json!({ "from": "ended", "to": "running", "paused_until": null }),
        )?;
    }
    Ok(())
}

/// Checks that `agent` is registered and records that it was seen now, leaving its state
/// alone: for the state change itself and for calls made on the agent's behalf.
pub(crate) fn note_agent_seen(cx: &mut Ctx<'_>, agent: &AgentId) -> Result<()> {
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
