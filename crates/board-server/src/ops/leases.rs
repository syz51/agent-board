use board_proto::{
    AgentId, CreateLeases, EventType, Lease, LeaseResult, Overlap, TouchResult, Touches,
};
use globset::{GlobBuilder, GlobMatcher};
use rusqlite::{OptionalExtension, params};

use crate::db::{Ctx, parse_id, ts};
use crate::error::{Result, forbidden, invalid, not_found};
use crate::ops::{expires, touch_agent};
use crate::store::{LEASE_COLUMNS, handle_of, is_live, lease_row, live_leases};
use crate::text::{check_path, normalize_repo};

/// Longest lease a request may ask for.
const MAX_LEASE_SECS: u32 = 24 * 3600;

fn matcher(glob: &str) -> Option<GlobMatcher> {
    GlobBuilder::new(glob)
        .literal_separator(true)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

/// Does `glob` cover the repo-relative `path`?
pub(crate) fn glob_matches(glob: &str, path: &str) -> bool {
    glob == path || matcher(glob).is_some_and(|m| m.is_match(path))
}

/// The directory part of a glob before its first wildcard, such as `crates/board/` for
/// `crates/board/*.rs`.
fn static_dir(glob: &str) -> &str {
    let literal = glob.find(['*', '?', '[', '{']).map_or(glob, |i| &glob[..i]);
    literal.rfind('/').map_or("", |i| &literal[..=i])
}

/// Could some path match both globs? Exact for literal paths; for two patterns, true when
/// either matches the other's text or a recursive (`**`) glob covers the other's directory.
pub(crate) fn globs_overlap(a: &str, b: &str) -> bool {
    if glob_matches(a, b) || glob_matches(b, a) {
        return true;
    }
    let (da, db) = (static_dir(a), static_dir(b));
    (db.starts_with(da) && a.contains("**")) || (da.starts_with(db) && b.contains("**"))
}

pub(crate) struct ValidLeases {
    pub req: CreateLeases,
}

pub(crate) fn validate_create(mut req: CreateLeases) -> Result<ValidLeases> {
    req.repo = normalize_repo(&req.repo)?;
    if req.globs.is_empty() || req.globs.len() > 50 {
        return Err(invalid(
            "send 1 to 50 globs",
            "lease the paths you are about to edit, such as crates/board-server/**",
        ));
    }
    for glob in &req.globs {
        check_path("glob", glob)?;
        if matcher(glob).is_none() {
            return Err(invalid(
                format!("glob {glob:?} is not a valid glob"),
                "use * within a directory, ** across directories, such as src/**/*.rs",
            ));
        }
    }
    // Drop repeats anywhere in the list, keeping the first of each.
    let mut seen = std::collections::HashSet::new();
    req.globs.retain(|glob| seen.insert(glob.clone()));
    if let Some(ttl) = req.ttl_secs
        && (ttl == 0 || ttl > MAX_LEASE_SECS)
    {
        return Err(invalid(
            format!("ttl_secs {ttl} is out of range"),
            format!("send 1 to {MAX_LEASE_SECS} seconds, or leave it out for the default"),
        ));
    }
    Ok(ValidLeases { req })
}

pub(crate) fn create(cx: &mut Ctx<'_>, valid: ValidLeases) -> Result<LeaseResult> {
    let req = valid.req;
    touch_agent(cx, &req.agent)?;
    let board = match &req.task {
        Some(task) => Some(
            cx.db
                .query_row(
                    "SELECT board FROM tasks WHERE id = ?1",
                    [task.as_str()],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .ok_or_else(|| {
                    invalid(
                        format!("task {task} does not exist"),
                        "check the task id, or leave task out",
                    )
                })
                .and_then(|b| Ok(parse_id::<board_proto::BoardSlug>(&b)?))?,
        ),
        None => None,
    };
    let ttl_ms = req
        .ttl_secs
        .map_or(cx.s.lease_ms(), |s| i64::from(s) * 1000);
    let until = expires(cx, ttl_ms);
    let now = cx.now_ms();
    let mut leases = Vec::with_capacity(req.globs.len());
    for glob in &req.globs {
        let existing: Option<i64> = cx
            .db
            .query_row(
                "SELECT id FROM leases WHERE agent = ?1 AND repo = ?2 AND glob = ?3",
                params![req.agent.as_str(), req.repo, glob],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => {
                cx.db.execute(
                    "UPDATE leases SET expires_at = ?2, task = ?3 WHERE id = ?1",
                    params![id, until, req.task.as_ref().map(|t| t.as_str())],
                )?;
                id
            }
            None => {
                cx.db.execute(
                    "INSERT INTO leases (agent, task, repo, glob, expires_at, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        req.agent.as_str(),
                        req.task.as_ref().map(|t| t.as_str()),
                        req.repo,
                        glob,
                        until,
                        now
                    ],
                )?;
                cx.db.last_insert_rowid()
            }
        };
        leases.push(cx.db.query_row(
            &format!("SELECT {LEASE_COLUMNS} FROM leases WHERE id = ?1"),
            [id],
            lease_row,
        )?);
    }
    let mut conflicts: Vec<Lease> = Vec::new();
    for lease in live_leases(cx, &req.repo)? {
        if lease.agent == req.agent || !is_live(cx, lease.agent.as_str())? {
            continue;
        }
        if req.globs.iter().any(|g| globs_overlap(g, &lease.glob)) {
            conflicts.push(lease);
        }
    }
    cx.emit(
        board.as_ref(),
        req.agent.as_str(),
        EventType::LeaseCreated,
        req.task.as_ref(),
        serde_json::json!({
            "repo": req.repo,
            "globs": req.globs,
            "ids": leases.iter().map(|l| l.id).collect::<Vec<_>>(),
            "conflicts": conflicts.len(),
        }),
    )?;
    Ok(LeaseResult { leases, conflicts })
}

pub(crate) fn release(cx: &mut Ctx<'_>, id: i64, agent: &AgentId) -> Result<()> {
    let lease = cx
        .db
        .query_row(
            &format!("SELECT {LEASE_COLUMNS} FROM leases WHERE id = ?1"),
            [id],
            lease_row,
        )
        .optional()?
        .ok_or_else(|| {
            not_found(
                format!("lease {id} does not exist (released or expired)"),
                "nothing to release",
            )
        })?;
    if lease.agent != *agent {
        return Err(forbidden(
            format!("lease {id} belongs to {}", lease.agent),
            "only the agent holding a lease may release it",
        ));
    }
    touch_agent(cx, agent)?;
    cx.db.execute("DELETE FROM leases WHERE id = ?1", [id])?;
    cx.emit(
        None,
        agent.as_str(),
        EventType::LeaseReleased,
        lease.task.as_ref(),
        serde_json::json!({ "id": id, "repo": lease.repo, "glob": lease.glob }),
    )?;
    Ok(())
}

pub(crate) fn validate_touches(mut req: Touches) -> Result<Touches> {
    req.repo = normalize_repo(&req.repo)?;
    if req.paths.is_empty() || req.paths.len() > 100 {
        return Err(invalid(
            "send 1 to 100 paths",
            "send the files you changed, relative to the repo root",
        ));
    }
    for path in &req.paths {
        check_path("path", path)?;
    }
    req.paths.sort();
    req.paths.dedup();
    Ok(req)
}

/// Records touched files, renews the agent's leases in the repo, and reports peers that
/// leased or touched the same paths recently.
pub(crate) fn touch(cx: &mut Ctx<'_>, req: Touches) -> Result<TouchResult> {
    touch_agent(cx, &req.agent)?;
    let now = cx.now_ms();
    for path in &req.paths {
        cx.db.execute(
            "INSERT INTO touches (agent, repo, path, at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (agent, repo, path) DO UPDATE SET at = ?4",
            params![req.agent.as_str(), req.repo, path, now],
        )?;
    }
    cx.db.execute(
        "UPDATE leases SET expires_at = MAX(expires_at, ?4) \
         WHERE agent = ?1 AND repo = ?2 AND expires_at > ?3",
        params![
            req.agent.as_str(),
            req.repo,
            now,
            expires(cx, cx.s.lease_ms())
        ],
    )?;
    let overlaps = overlaps(cx, &req.agent, &req.repo, &req.paths)?;
    cx.emit(
        None,
        req.agent.as_str(),
        EventType::FilesTouched,
        None,
        serde_json::json!({
            "repo": req.repo,
            "paths": req.paths,
            "overlaps": overlaps.len(),
        }),
    )?;
    Ok(TouchResult { overlaps })
}

/// Live peers' leases covering `paths`, and peers' touches of them within the window.
fn overlaps(cx: &Ctx<'_>, me: &AgentId, repo: &str, paths: &[String]) -> Result<Vec<Overlap>> {
    let mut out = Vec::new();
    let leases: Vec<(String, String, i64)> = cx
        .db
        .prepare_cached(
            "SELECT agent, glob, created_at FROM leases \
             WHERE repo = ?1 AND expires_at > ?2 AND agent != ?3 ORDER BY id",
        )?
        .query_map(params![repo, cx.now_ms(), me.as_str()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let window_start = cx.now_ms() - cx.s.touch_ms();
    for path in paths {
        for (agent, glob, at) in &leases {
            if glob_matches(glob, path) && is_live(cx, agent)? {
                out.push(Overlap {
                    path: path.clone(),
                    agent: parse_id(agent)?,
                    handle: handle_of(cx, agent)?,
                    via: "lease".to_owned(),
                    at: ts(*at),
                });
            }
        }
        let touches: Vec<(String, i64)> = cx
            .db
            .prepare_cached(
                "SELECT agent, at FROM touches WHERE repo = ?1 AND path = ?2 AND agent != ?3 \
                 AND at > ?4 ORDER BY at DESC",
            )?
            .query_map(params![repo, path, me.as_str(), window_start], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (agent, at) in touches {
            out.push(Overlap {
                path: path.clone(),
                agent: parse_id(&agent)?,
                handle: handle_of(cx, &agent)?,
                via: "touch".to_owned(),
                at: ts(at),
            });
        }
    }
    Ok(out)
}

/// Deletes expired leases, recording `lease_expired` for each.
pub(crate) fn sweep_expired(cx: &mut Ctx<'_>) -> Result<usize> {
    let expired: Vec<Lease> = cx
        .db
        .prepare_cached(&format!(
            "SELECT {LEASE_COLUMNS} FROM leases WHERE expires_at <= ?1 ORDER BY id"
        ))?
        .query_map([cx.now_ms()], lease_row)?
        .collect::<rusqlite::Result<_>>()?;
    for lease in &expired {
        cx.db
            .execute("DELETE FROM leases WHERE id = ?1", [lease.id])?;
        cx.emit(
            None,
            "server",
            EventType::LeaseExpired,
            lease.task.as_ref(),
            serde_json::json!({
                "id": lease.id,
                "agent": lease.agent,
                "repo": lease.repo,
                "glob": lease.glob,
            }),
        )?;
    }
    Ok(expired.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_overlap() {
        assert!(globs_overlap("crates/**", "crates/board/*.rs"));
        assert!(globs_overlap("src/*.rs", "src/lib.rs"));
        assert!(globs_overlap("src/lib.rs", "src/lib.rs"));
        assert!(globs_overlap("**", "web/index.html"));
        assert!(!globs_overlap("src/*.rs", "src/*.toml"));
        assert!(!globs_overlap("src/*", "src/a/b.rs"));
        assert!(!globs_overlap("web/**", "crates/**"));
        assert!(glob_matches(
            "crates/board-server/**",
            "crates/board-server/src/lib.rs"
        ));
        assert!(!glob_matches("src/*.rs", "src/a/b.rs"));
    }
}
