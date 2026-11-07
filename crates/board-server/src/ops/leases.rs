use std::collections::HashSet;

use board_proto::{
    AgentId, CreateLeases, EventType, Lease, LeaseResult, Overlap, TOUCH_PATHS_MAX, TouchResult,
    Touches,
};
use globset::{GlobBuilder, GlobMatcher};
use rusqlite::{OptionalExtension, params};

use crate::db::{Ctx, parse_id, ts};
use crate::error::{Result, forbidden, invalid, not_found};
use crate::ops::{expires, touch_agent};
use crate::store::{LEASE_COLUMNS, get_board, handle_of, is_live, lease_row, live_leases};
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

/// How many delegations deep [`delegation_family`] follows, so a cycle of `t3_parent`
/// ids ends.
const DELEGATION_DEPTH: i64 = 8;

/// The query of [`delegation_family`]: `?1` the agent, `?2` the depth. The `down` join
/// compares the indexed expression `agents_t3_parent` with the parent's id without `t3:`, so
/// it is an index search rather than a scan of every agent per step.
pub(crate) const DELEGATION_FAMILY: &str = "WITH RECURSIVE \
     up(id, depth) AS ( \
       SELECT ?1, 0 \
       UNION \
       SELECT 't3:' || json_extract(a.native_ids, '$.t3_parent'), up.depth + 1 \
       FROM agents a JOIN up ON a.id = up.id \
       WHERE json_extract(a.native_ids, '$.t3_parent') IS NOT NULL \
         AND up.depth < ?2), \
     down(id, depth) AS ( \
       SELECT ?1, 0 \
       UNION \
       SELECT a.id, down.depth + 1 \
       FROM down JOIN agents a \
         ON json_extract(a.native_ids, '$.t3_parent') = substr(down.id, 4) \
       WHERE substr(down.id, 1, 3) = 't3:' AND down.depth < ?2) \
     SELECT id FROM up UNION SELECT id FROM down";

/// The T3 threads that delegated `agent`, up the chain, and those it delegated, down every
/// branch (each `t3_parent` native id names the thread that delegated it), without `agent`.
/// A thread and those it delegates do one piece of work, so their edits and leases never
/// overlap each other. Siblings are not family: parallel children can collide.
pub(crate) fn delegation_family(cx: &Ctx<'_>, agent: &str) -> Result<HashSet<String>> {
    let mut family: HashSet<String> = cx
        .db
        .prepare_cached(DELEGATION_FAMILY)?
        .query_map(params![agent, DELEGATION_DEPTH], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    family.remove(agent);
    Ok(family)
}

/// A lease glob compiled once, to compare with many paths or globs.
pub(crate) struct Glob<'a> {
    text: &'a str,
    matcher: Option<GlobMatcher>,
}

impl<'a> Glob<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        Self {
            text,
            matcher: matcher(text),
        }
    }

    /// Does it cover the repo-relative `path`?
    pub(crate) fn matches(&self, path: &str) -> bool {
        self.text == path || self.matcher.as_ref().is_some_and(|m| m.is_match(path))
    }

    /// Could some path match both globs? Exact for literal paths; for two patterns, true
    /// when either matches the other's text or a recursive (`**`) glob covers the other's
    /// directory.
    pub(crate) fn overlaps(&self, other: &Glob<'_>) -> bool {
        if self.matches(other.text) || other.matches(self.text) {
            return true;
        }
        let (a, b) = (self.text, other.text);
        let (da, db) = (static_dir(a), static_dir(b));
        (db.starts_with(da) && a.contains("**")) || (da.starts_with(db) && b.contains("**"))
    }
}

/// The directory part of a glob before its first wildcard, such as `crates/board/` for
/// `crates/board/*.rs`.
fn static_dir(glob: &str) -> &str {
    let literal = glob.find(['*', '?', '[', '{']).map_or(glob, |i| &glob[..i]);
    literal.rfind('/').map_or("", |i| &literal[..=i])
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
    let mut seen = HashSet::new();
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
    let mut req = valid.req;
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
    if let Some(board) = &board {
        req.repo = lease_repo(cx, board, req.task.as_ref(), req.repo)?;
    }
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
    let family = delegation_family(cx, req.agent.as_str())?;
    let globs: Vec<Glob<'_>> = req.globs.iter().map(|g| Glob::new(g)).collect();
    for lease in live_leases(cx, &req.repo)? {
        if lease.agent == req.agent
            || !is_live(cx, lease.agent.as_str())?
            || family.contains(lease.agent.as_str())
        {
            continue;
        }
        let theirs = Glob::new(&lease.glob);
        if globs.iter().any(|g| g.overlaps(&theirs)) {
            drop(theirs);
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

/// The repo a lease for a task on `board` goes to. Peers working the task are on the
/// board's repos, so a lease from a checkout of another repo would warn nobody: it moves to
/// the board's repo when the board lists one, and is refused when it lists several. A board
/// without repos takes the caller's.
fn lease_repo(
    cx: &Ctx<'_>,
    board: &board_proto::BoardSlug,
    task: Option<&board_proto::TaskId>,
    repo: String,
) -> Result<String> {
    let repos = get_board(cx, board)?.repos;
    match repos.as_slice() {
        [] => Ok(repo),
        list if list.contains(&repo) => Ok(repo),
        [only] => Ok(only.clone()),
        list => Err(invalid(
            format!(
                "board {board} of task {} lists {}, not {repo}",
                task.map_or("", |t| t.as_str()),
                list.join(", ")
            ),
            format!(
                "run lease_paths from a checkout of one of {}, or leave task out to lease in {repo}",
                list.join(", ")
            ),
        )),
    }
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
    if req.paths.is_empty() || req.paths.len() > TOUCH_PATHS_MAX {
        return Err(invalid(
            format!("send 1 to {TOUCH_PATHS_MAX} paths"),
            "send the files you changed, relative to the repo root",
        ));
    }
    for path in &req.paths {
        check_path("path", path)?;
    }
    req.paths.sort();
    req.paths.dedup();
    if let Some(extra) = req
        .modified
        .keys()
        .find(|path| req.paths.binary_search(path).is_err())
    {
        return Err(invalid(
            format!("modified names {extra:?}, which is not in paths"),
            "send modification times only for paths in the request",
        ));
    }
    Ok(req)
}

/// SQL that is true when a peer's touch `t` and the agent `me`'s touch of the same path
/// were both found by shell scans on one device with the same modification time: one write
/// that happened during both agents' shell calls, so neither can say whose it was. Such a
/// pair never counts as an overlap between the two; other peers still see both touches.
pub(crate) fn shared_scan(me: &str) -> String {
    format!(
        "(t.scanned = 1 AND EXISTS (SELECT 1 FROM touches m WHERE m.agent = {me} \
           AND m.repo = t.repo AND m.path = t.path AND m.scanned = 1 \
           AND m.modified = t.modified) \
         AND t.agent IN (SELECT id FROM agents \
           WHERE device = (SELECT device FROM agents WHERE id = {me})))"
    )
}

/// Records touched files, renews the agent's leases in the repo, and reports peers that
/// leased or touched the same paths recently.
pub(crate) fn touch(cx: &mut Ctx<'_>, req: Touches) -> Result<TouchResult> {
    touch_agent(cx, &req.agent)?;
    let now = cx.now_ms();
    let me = req.agent.as_str();
    let mut recorded = Vec::with_capacity(req.paths.len());
    for path in &req.paths {
        let modified = req.modified.get(path).map(|at| at.timestamp_millis());
        // A scanned path with the time of an edit-tool touch by a peer on the caller's
        // device (the only peers that can share its work tree) is the peer's write, made
        // during the caller's shell call. When the peer's touch comes second, it removes
        // the scanned one instead.
        if let Some(modified) = modified {
            let peers = "agent != ?3 AND agent IN (SELECT id FROM agents \
                 WHERE device = (SELECT device FROM agents WHERE id = ?3))";
            if req.scanned {
                let peer_wrote: bool = cx.db.query_row(
                    &format!(
                        "SELECT EXISTS (SELECT 1 FROM touches WHERE repo = ?1 AND path = ?2 \
                         AND scanned = 0 AND modified = ?4 AND {peers})"
                    ),
                    params![req.repo, path, me, modified],
                    |r| r.get(0),
                )?;
                if peer_wrote {
                    continue;
                }
            } else {
                cx.db.execute(
                    &format!(
                        "DELETE FROM touches WHERE repo = ?1 AND path = ?2 \
                         AND scanned = 1 AND modified = ?4 AND {peers}"
                    ),
                    params![req.repo, path, me, modified],
                )?;
            }
        }
        // A scanned report never replaces the caller's own edit-tool touch, which a peer's
        // matching write could then remove.
        let written = cx.db.execute(
            "INSERT INTO touches (agent, repo, path, at, scanned, modified) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (agent, repo, path) DO UPDATE \
             SET at = ?4, scanned = ?5, modified = ?6 \
             WHERE touches.scanned = 1 OR excluded.scanned = 0",
            params![me, req.repo, path, now, req.scanned, modified],
        )?;
        if written > 0 {
            recorded.push(path.clone());
        }
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
    if !recorded.is_empty() {
        cx.emit(
            None,
            me,
            EventType::FilesTouched,
            None,
            serde_json::json!({
                "repo": req.repo,
                "paths": recorded,
                "overlaps": overlaps.iter().filter(|o| recorded.contains(&o.path)).count(),
            }),
        )?;
    }
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
    let globs: Vec<Glob<'_>> = leases.iter().map(|(_, g, _)| Glob::new(g)).collect();
    let family = delegation_family(cx, me.as_str())?;
    let window_start = cx.now_ms() - cx.s.touch_ms();
    let touches_sql = format!(
        "SELECT agent, at FROM touches t WHERE repo = ?1 AND path = ?2 AND agent != ?3 \
         AND at > ?4 AND NOT {} ORDER BY at DESC",
        shared_scan("?3")
    );
    for path in paths {
        for ((agent, _, at), glob) in leases.iter().zip(&globs) {
            if glob.matches(path) && is_live(cx, agent)? && !family.contains(agent) {
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
            .prepare_cached(&touches_sql)?
            .query_map(params![repo, path, me.as_str(), window_start], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        for (agent, at) in touches {
            if family.contains(&agent) {
                continue;
            }
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

    fn glob_matches(glob: &str, path: &str) -> bool {
        Glob::new(glob).matches(path)
    }

    fn globs_overlap(a: &str, b: &str) -> bool {
        Glob::new(a).overlaps(&Glob::new(b))
    }

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
