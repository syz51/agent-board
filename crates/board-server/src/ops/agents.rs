use board_proto::{
    Agent, AgentId, AgentRole, AgentSource, AgentState, AgentStateUpdate, EventType, Handoff,
    PutHandoff, RegisterAgent,
};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::auth::hex;
use crate::db::{Ctx, enum_str, parse_id};
use crate::error::{Result, conflict, invalid};
use crate::ops::{expires, join_board, touch_agent};
use crate::store::{agent_missing, find_handoff, get_agent, load_agents};
use crate::text::{check_line, check_text, normalize_repo};

/// A validated registration.
pub(crate) struct Registration {
    pub req: RegisterAgent,
    pub repo: Option<String>,
}

pub(crate) fn validate_register(req: RegisterAgent) -> Result<Registration> {
    if let Some(title) = &req.title {
        check_line("title", title, 200)?;
    }
    if let Some(branch) = &req.branch {
        check_line("branch", branch, 200)?;
    }
    if let Some(cwd) = &req.cwd {
        check_line("cwd", cwd, 1000)?;
    }
    if req.native_ids.len() > 20 {
        return Err(invalid(
            "native_ids has more than 20 entries",
            "send at most 20 native ids",
        ));
    }
    for (key, value) in &req.native_ids {
        check_line("native_ids key", key, 64)?;
        check_line("native_ids value", value, 300)?;
    }
    if req.forked_from.as_ref() == Some(&req.id) {
        return Err(invalid(
            "forked_from names the agent itself",
            "send the id of the agent this one was forked from, or leave it out",
        ));
    }
    let repo = req.repo.as_deref().map(normalize_repo).transpose()?;
    Ok(Registration { req, repo })
}

/// The handle for a new agent: its source plus the shortest unused hex prefix (at least
/// four characters) of the SHA-256 of its id. Stored, so it never changes.
fn new_handle(cx: &Ctx<'_>, id: &AgentId) -> Result<String> {
    let prefix = match id.source() {
        AgentSource::T3 => "t3",
        AgentSource::Claude => "claude",
        AgentSource::Codex => "codex",
    };
    let digest = hex(&Sha256::digest(id.as_str().as_bytes()));
    for len in 4..=digest.len() {
        let handle = format!("{prefix}:{}", &digest[..len]);
        if !cx.exists("SELECT 1 FROM agents WHERE handle = ?1", [&handle])? {
            return Ok(handle);
        }
    }
    unreachable!("a full SHA-256 is unique among agents with distinct ids")
}

pub(crate) fn register(cx: &mut Ctx<'_>, reg: Registration) -> Result<Agent> {
    let Registration { req, repo } = reg;
    let native_ids = serde_json::to_string(&req.native_ids).expect("maps serialize");
    let previous: Option<(Option<String>, Option<String>)> = cx
        .db
        .query_row(
            "SELECT repo, branch FROM agents WHERE id = ?1",
            [req.id.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    // The session-start hook registers every agent as a worker; an agent named as a
    // board's coordinator keeps that role.
    let role = if cx.exists(
        "SELECT 1 FROM boards WHERE coordinator = ?1",
        [req.id.as_str()],
    )? {
        AgentRole::Coordinator
    } else {
        req.role
    };
    let now = cx.now_ms();
    let is_new = previous.is_none();
    if is_new {
        let handle = new_handle(cx, &req.id)?;
        // A new agent's cursor starts at the present: older events are not news to it.
        let cursor = cx.max_seq()?;
        cx.db.execute(
            "INSERT INTO agents (id, handle, runtime, device, role, title, repo, branch, cwd, \
             native_ids, forked_from, state, paused_until, last_seen, created_at, cursor) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'running', NULL, ?12, ?12, ?13)",
            params![
                req.id.as_str(),
                handle,
                enum_str(&req.runtime),
                req.device.as_str(),
                enum_str(&role),
                req.title,
                repo,
                req.branch,
                req.cwd,
                native_ids,
                req.forked_from.as_ref().map(AgentId::as_str),
                now,
                cursor,
            ],
        )?;
    } else {
        cx.db.execute(
            // A title or native id left out keeps the stored one; repo, branch and cwd say
            // where the agent runs now, so they are replaced.
            "UPDATE agents SET runtime = ?2, device = ?3, role = ?4, title = COALESCE(?5, title), \
             repo = ?6, branch = ?7, cwd = ?8, native_ids = json_patch(native_ids, ?9), \
             forked_from = COALESCE(?10, forked_from), state = 'running', paused_until = NULL, \
             last_seen = ?11 WHERE id = ?1",
            params![
                req.id.as_str(),
                enum_str(&req.runtime),
                req.device.as_str(),
                enum_str(&role),
                req.title,
                repo,
                req.branch,
                req.cwd,
                native_ids,
                req.forked_from.as_ref().map(AgentId::as_str),
                now,
            ],
        )?;
    }
    let moved = previous.is_some_and(|(r, b)| r != repo || b != req.branch);
    if is_new || moved {
        cx.emit(
            None,
            req.id.as_str(),
            EventType::AgentRegistered,
            None,
            serde_json::json!({
                "new": is_new,
                "runtime": req.runtime,
                "repo": repo,
                "branch": req.branch,
                "forked_from": req.forked_from,
            }),
        )?;
    }
    if let Some(repo) = &repo {
        for board in boards_for_repo(cx, repo)? {
            join_board(cx, &board, &req.id)?;
        }
    }
    get_agent(cx, &req.id)
}

/// Active boards that cover `repo`.
pub(crate) fn boards_for_repo(cx: &Ctx<'_>, repo: &str) -> Result<Vec<board_proto::BoardSlug>> {
    Ok(cx
        .db
        .prepare_cached(
            "SELECT b.slug FROM boards b, json_each(b.repos) r \
             WHERE r.value = ?1 AND b.status = 'active' ORDER BY b.slug",
        )?
        .query_map([repo], |r| parse_id(&r.get::<_, String>(0)?))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn list(
    cx: &Ctx<'_>,
    board: Option<&board_proto::BoardSlug>,
    state: Option<AgentState>,
) -> Result<Vec<Agent>> {
    let board = board.map(|b| b.as_str().to_owned());
    let state = state.map(|s| enum_str(&s));
    let mut filter = vec!["1 = 1"];
    let mut params: crate::store::NamedParams<'_> = Vec::new();
    if let Some(board) = &board {
        filter.push("id IN (SELECT agent FROM board_members WHERE board = :board)");
        params.push((":board", board));
    }
    if let Some(state) = &state {
        filter.push("state = :state");
        params.push((":state", state));
    }
    load_agents(cx, &filter.join(" AND "), &params)
}

pub(crate) fn validate_state(update: &AgentStateUpdate) -> Result<()> {
    if update.paused_until.is_some() && update.state != AgentState::Paused {
        return Err(invalid(
            "paused_until is only for state paused",
            "send state=paused with paused_until, or leave paused_until out",
        ));
    }
    Ok(())
}

/// Heartbeat and lifecycle. Renews the agent's leases and coordinator lease; `ended`
/// releases them instead.
pub(crate) fn set_state(cx: &mut Ctx<'_>, id: &AgentId, update: AgentStateUpdate) -> Result<Agent> {
    let before: String = cx
        .db
        .query_row(
            "SELECT state FROM agents WHERE id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| agent_missing(id))?;
    touch_agent(cx, id)?;
    let state = enum_str(&update.state);
    cx.db.execute(
        "UPDATE agents SET state = ?2, paused_until = ?3 WHERE id = ?1",
        params![
            id.as_str(),
            state,
            update.paused_until.map(|p| p.timestamp_millis())
        ],
    )?;
    let now = cx.now_ms();
    if update.state == AgentState::Ended {
        release_all(cx, id)?;
    } else {
        cx.db.execute(
            "UPDATE leases SET expires_at = MAX(expires_at, ?3) WHERE agent = ?1 AND expires_at > ?2",
            params![id.as_str(), now, expires(cx, cx.s.lease_ms())],
        )?;
        cx.db.execute(
            "UPDATE boards SET coordinator_expires_at = ?3 \
             WHERE coordinator = ?1 AND coordinator_expires_at > ?2",
            params![id.as_str(), now, expires(cx, cx.s.coordinator_ms())],
        )?;
    }
    if before != state || update.state == AgentState::Paused {
        cx.emit(
            None,
            id.as_str(),
            EventType::AgentStateChanged,
            None,
            serde_json::json!({
                "from": before,
                "to": update.state,
                "paused_until": update.paused_until,
            }),
        )?;
    }
    get_agent(cx, id)
}

/// Releases the agent's file leases and coordinator leases.
fn release_all(cx: &mut Ctx<'_>, id: &AgentId) -> Result<()> {
    let leases: Vec<(i64, String, String)> = cx
        .db
        .prepare_cached("SELECT id, repo, glob FROM leases WHERE agent = ?1 ORDER BY id")?
        .query_map([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    cx.db
        .execute("DELETE FROM leases WHERE agent = ?1", [id.as_str()])?;
    for (lease, repo, glob) in leases {
        cx.emit(
            None,
            id.as_str(),
            EventType::LeaseReleased,
            None,
            serde_json::json!({ "id": lease, "repo": repo, "glob": glob }),
        )?;
    }
    let boards: Vec<String> = cx
        .db
        .prepare_cached("SELECT slug FROM boards WHERE coordinator = ?1")?
        .query_map([id.as_str()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for board in boards {
        cx.db.execute(
            "UPDATE boards SET coordinator = NULL, coordinator_device = NULL, \
             coordinator_expires_at = NULL, updated_at = ?2 WHERE slug = ?1",
            params![board, cx.now_ms()],
        )?;
        let slug = parse_id(&board)?;
        cx.emit(
            Some(&slug),
            id.as_str(),
            EventType::BoardUpdated,
            None,
            serde_json::json!({ "fields": ["coordinator"], "coordinator": null }),
        )?;
    }
    Ok(())
}

pub(crate) fn validate_handoff(req: &PutHandoff, max: usize) -> Result<()> {
    check_text("now", &req.now, max)?;
    check_text("next", &req.next, max)?;
    check_text("blockers", &req.blockers, max)?;
    if req.now.trim().is_empty() && req.next.trim().is_empty() {
        return Err(invalid(
            "handoff now and next are both empty",
            "say what you are doing (now) and the first concrete next action (next)",
        ));
    }
    Ok(())
}

pub(crate) fn put_handoff(cx: &mut Ctx<'_>, id: &AgentId, req: PutHandoff) -> Result<Handoff> {
    touch_agent(cx, id)?;
    let current = find_handoff(cx, id)?;
    let version = match (&current, req.version) {
        (None, None | Some(0)) => 1,
        (Some(h), Some(v)) if v == h.version => h.version + 1,
        (Some(h), _) => {
            return Err(conflict(
                format!(
                    "the handoff of {id} is at version {}; you sent {}",
                    h.version,
                    req.version
                        .map_or_else(|| "none".to_owned(), |v| v.to_string())
                ),
                format!(
                    "re-read the handoff (GET /v1/agents/{id}/handoff) and retry with version {}",
                    h.version
                ),
            ));
        }
        (None, Some(v)) => {
            return Err(conflict(
                format!("{id} has no handoff yet; you sent version {v}"),
                "leave version out to create the first handoff",
            ));
        }
    };
    cx.db.execute(
        "INSERT INTO handoffs (agent, now, next, blockers, version, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (agent) DO UPDATE SET now = ?2, next = ?3, blockers = ?4, version = ?5, \
         updated_at = ?6",
        params![
            id.as_str(),
            req.now,
            req.next,
            req.blockers,
            version,
            cx.now_ms()
        ],
    )?;
    cx.emit(
        None,
        id.as_str(),
        EventType::HandoffUpdated,
        None,
        serde_json::json!({ "version": version }),
    )?;
    Ok(find_handoff(cx, id)?.expect("just written"))
}
