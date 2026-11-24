//! Unified runs, fleet-wide conflict locks, lease expiry sweeper, and dispatcher-agnostic
//! standing review verification (`docs/SCHEDULER_DESIGN.md`, Section 3.3).

use std::collections::HashMap;

use board_proto::{
    Agent, AgentId, AgentSource, AgentState, BoardSlug, CancelRunRequest, CancelRunResponse,
    ConflictKeySpec, ConflictLock, CreateRun, DeviceName, EventType, Note, PushRunEvents,
    RUN_CANCEL_REASON_CHARS_MAX, RUN_EVENT_META_BYTES_MAX, RUN_EVENT_SUMMARY_CHARS_MAX,
    RUN_EVENTS_BATCH_MAX, RUN_EVENTS_CAP, RUN_EVENTS_LIMIT_MAX, RUN_STEER_MESSAGE_CHARS_MAX,
    RUNS_LIMIT_DEFAULT, RUNS_LIMIT_MAX, Run, RunEventKind, RunEventRecord, RunFilter, RunId,
    RunKind, RunOrigin, RunState, SteerRunRequest, SteerRunResponse, StickyWake, TaskId,
    TaskStatus, Timestamp, UpdateRun,
};
use rusqlite::{OptionalExtension, Row, params};
use sha2::{Digest, Sha256};

use crate::auth::{hex, random_hex};
use crate::db::{Ctx, enum_str, parse_enum, parse_id, parse_json, ts};
use crate::error::{Result, bad_transition, forbidden, invalid, not_found, refused};
use crate::ops::devices::check_token;
use crate::ops::expires;
use crate::ops::tasks::check_conflict_keys;
use crate::store::{NamedParams, agent_by_id_or_handle, get_board_uncounted, get_task, json_ids};
use crate::text::{check_line, check_line_or, check_text};

/// Longest renewable run lease a request may ask for (24 hours).
const MAX_RUN_LEASE_SECS: u64 = 24 * 3600;
/// Maximum weighted cost units per run.
const RUN_COST_UNITS_MAX: u32 = 64;
/// Maximum serialized byte size of `Run::outcome`.
const RUN_OUTCOME_BYTES_MAX: usize = 65_536;
/// Maximum length of `UpdateRun::reason`, in characters.
const RUN_REASON_CHARS_MAX: usize = 500;
/// Deterministic ID prefix for interactive runs auto-upserted from `/v1/agents` hooks.
const INTERACTIVE_RUN_PREFIX: &str = "run-interactive-";
/// Standard weighted cost units for an active interactive session.
const INTERACTIVE_RUN_COST_UNITS: u32 = 2;
/// Maximum length of `Run::attach_hint`, matching [`check_optional_fields`].
const ATTACH_HINT_CHARS_MAX: usize = 500;
/// Retention window for `run_events` of terminal unarchived runs (7 days after `ended_at`).
const RUN_EVENTS_RETENTION_MS: i64 = 7 * 86_400_000;
/// Grace window for `run_events` after a terminal run records `transcript_uri` (1 hour).
const RUN_EVENTS_ARCHIVED_GRACE_MS: i64 = 3_600_000;

/// SQL predicate over alias `r` for a live managed run that grants its `agent_id`
/// coordinator rights on its board without requiring a per-board coordinator lease.
/// Expects named parameter `:now`.
pub(crate) const LIVE_RUN_RIGHTS: &str = "r.origin = 'managed' \
     AND r.kind IN ('review', 'verdict', 'triage', 'goal', 'merge', 'housekeeping') \
     AND r.state IN ('leased', 'starting', 'running', 'waiting_external') \
     AND r.lease_expires_at IS NOT NULL \
     AND r.lease_expires_at > :now";

pub(crate) const RUN_COLUMNS: &str = "r.id, r.task_id, r.board, r.kind, r.origin, r.device, r.runner, \
     r.agent_id, r.state, r.lease_expires_at, r.worktree_branch, r.cwd, r.title, \
     r.attach_hint, r.cost_units, r.policy_version, r.outcome_json, r.transcript_uri, \
     r.transcript_sha256, r.created_at, r.started_at, r.ended_at, r.updated_at";

pub(crate) fn run_row(r: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: parse_id(&r.get::<_, String>(0)?)?,
        task_id: r
            .get::<_, Option<String>>(1)?
            .map(|s| parse_id(&s))
            .transpose()?,
        board: r
            .get::<_, Option<String>>(2)?
            .map(|s| parse_id(&s))
            .transpose()?,
        kind: parse_enum(&r.get::<_, String>(3)?)?,
        origin: parse_enum(&r.get::<_, String>(4)?)?,
        device: parse_id(&r.get::<_, String>(5)?)?,
        runner: r.get(6)?,
        agent_id: r
            .get::<_, Option<String>>(7)?
            .map(|s| parse_id(&s))
            .transpose()?,
        state: parse_enum(&r.get::<_, String>(8)?)?,
        lease_expires_at: r.get::<_, Option<i64>>(9)?.map(ts),
        worktree_branch: r.get(10)?,
        cwd: r.get(11)?,
        title: r.get(12)?,
        attach_hint: r.get(13)?,
        cost_units: r.get(14)?,
        policy_version: r.get(15)?,
        outcome: r
            .get::<_, Option<String>>(16)?
            .map(|s| parse_json(&s))
            .transpose()?,
        transcript_uri: r.get(17)?,
        transcript_sha256: r.get(18)?,
        conflict_locks: Vec::new(),
        created_at: ts(r.get(19)?),
        started_at: r.get::<_, Option<i64>>(20)?.map(ts),
        ended_at: r.get::<_, Option<i64>>(21)?.map(ts),
        updated_at: ts(r.get(22)?),
    })
}

fn complete_runs(cx: &Ctx<'_>, mut runs: Vec<Run>) -> Result<Vec<Run>> {
    if runs.is_empty() {
        return Ok(runs);
    }
    let ids = json_ids(runs.iter().map(|r| r.id.as_str()));
    let mut locks: HashMap<String, Vec<ConflictLock>> = HashMap::new();
    let mut stmt = cx.db.prepare_cached(
        "SELECT key, run_id, max_holders, acquired_at FROM conflict_locks \
         WHERE run_id IN (SELECT value FROM json_each(?1)) ORDER BY key",
    )?;
    let mut rows = stmt.query([&ids])?;
    while let Some(r) = rows.next()? {
        let run_id_str: String = r.get(1)?;
        let lock = ConflictLock {
            key: r.get(0)?,
            run_id: parse_id(&run_id_str)?,
            max_holders: r.get(2)?,
            acquired_at: ts(r.get(3)?),
        };
        locks.entry(run_id_str).or_default().push(lock);
    }
    for run in &mut runs {
        if let Some(held) = locks.remove(run.id.as_str()) {
            run.conflict_locks = held;
        }
    }
    Ok(runs)
}

fn allowed_targets(from: RunState) -> Vec<&'static str> {
    use RunState::*;
    [
        Queued,
        Leased,
        Starting,
        Running,
        WaitingExternal,
        Succeeded,
        Failed,
        Preempted,
        Orphaned,
        Cancelled,
    ]
    .into_iter()
    .filter(|&to| from.can_transition_to(to))
    .map(state_str)
    .collect()
}

fn state_str(state: RunState) -> &'static str {
    match state {
        RunState::Queued => "queued",
        RunState::Leased => "leased",
        RunState::Starting => "starting",
        RunState::Running => "running",
        RunState::WaitingExternal => "waiting_external",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Preempted => "preempted",
        RunState::Orphaned => "orphaned",
        RunState::Cancelled => "cancelled",
        RunState::Unknown => "unknown",
    }
}

fn check_lease_ttl(ttl: Option<u64>, at: Option<Timestamp>) -> Result<()> {
    if ttl.is_some() && at.is_some() {
        return Err(invalid(
            "both lease_ttl_secs and lease_expires_at were sent",
            "send either lease_ttl_secs or lease_expires_at, not both",
        ));
    }
    if let Some(secs) = ttl
        && !(1..=MAX_RUN_LEASE_SECS).contains(&secs)
    {
        return Err(invalid(
            format!("lease_ttl_secs={secs} is out of range"),
            format!("send lease_ttl_secs between 1 and {MAX_RUN_LEASE_SECS}"),
        ));
    }
    Ok(())
}

fn check_lease_expiry(now: Timestamp, at: Timestamp) -> Result<i64> {
    let now_ms = now.timestamp_millis();
    let at_ms = at.timestamp_millis();
    if at_ms <= now_ms {
        return Err(invalid(
            "lease_expires_at is not in the future",
            "send a lease_expires_at timestamp ahead of the server clock, or use lease_ttl_secs",
        ));
    }
    let max_ms =
        now_ms.saturating_add(i64::try_from(MAX_RUN_LEASE_SECS * 1000).unwrap_or(i64::MAX));
    if at_ms > max_ms {
        return Err(invalid(
            format!("lease_expires_at exceeds the {MAX_RUN_LEASE_SECS}s maximum TTL"),
            format!("send a lease at most {MAX_RUN_LEASE_SECS} seconds ahead"),
        ));
    }
    Ok(at_ms)
}

#[derive(Default)]
struct OptionalRunFields<'a> {
    worktree_branch: Option<&'a str>,
    cwd: Option<&'a str>,
    title: Option<&'a str>,
    attach_hint: Option<&'a str>,
    transcript_uri: Option<&'a str>,
    outcome: Option<&'a serde_json::Value>,
}

fn check_optional_fields(fields: &OptionalRunFields<'_>) -> Result<()> {
    if let Some(branch) = fields.worktree_branch {
        check_line("worktree_branch", branch, 200)?;
    }
    if let Some(cwd) = fields.cwd {
        check_line("cwd", cwd, 1000)?;
    }
    if let Some(title) = fields.title {
        check_line_or("title", title, board_proto::AGENT_TITLE_CHARS_MAX, None)?;
    }
    if let Some(hint) = fields.attach_hint {
        check_line("attach_hint", hint, 500)?;
    }
    if let Some(uri) = fields.transcript_uri {
        check_line("transcript_uri", uri, 1000)?;
    }
    if let Some(val) = fields.outcome {
        let len = serde_json::to_string(val)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if len > RUN_OUTCOME_BYTES_MAX {
            return Err(invalid(
                format!("outcome JSON is {len} bytes; the limit is {RUN_OUTCOME_BYTES_MAX}"),
                "store large payloads in object storage and link them via transcript_uri",
            ));
        }
    }
    Ok(())
}

/// Validates `POST /v1/runs` at the API boundary.
pub(crate) fn validate_create(mut req: CreateRun) -> Result<CreateRun> {
    if req.kind == RunKind::Unknown {
        return Err(invalid(
            "unknown run kind",
            "send kind as worker, review, verdict, triage, goal, merge, housekeeping, command or interactive",
        ));
    }
    if req.origin == RunOrigin::Unknown {
        return Err(invalid(
            "unknown run origin",
            "send origin as managed or interactive",
        ));
    }
    if let Some(state) = req.state {
        if state == RunState::Unknown {
            return Err(invalid(
                "unknown run state",
                "send initial state as queued, leased, starting or running",
            ));
        }
        if !matches!(
            state,
            RunState::Queued | RunState::Leased | RunState::Starting | RunState::Running
        ) {
            return Err(invalid(
                format!("a run cannot be created in state {}", state_str(state)),
                "create runs in queued, leased, starting or running, then transition via PATCH /v1/runs/{id}",
            ));
        }
    }
    req.runner = check_token("runner", &req.runner, 40)?;
    if !(1..=RUN_COST_UNITS_MAX).contains(&req.cost_units) {
        return Err(invalid(
            format!("cost_units={} is out of range", req.cost_units),
            format!("send cost_units between 1 and {RUN_COST_UNITS_MAX}"),
        ));
    }
    check_lease_ttl(req.lease_ttl_secs, req.lease_expires_at)?;
    if let Some(agent) = &req.agent_id
        && let Some(device) = agent.dispatch_device()
        && device != req.device
    {
        return Err(invalid(
            format!(
                "{agent} is the dispatch agent of device {device}, not of {}",
                req.device
            ),
            format!(
                "name device {}'s own dispatch agent or leave agent_id out",
                req.device
            ),
        ));
    }
    req.policy_version = req
        .policy_version
        .as_deref()
        .map(|pv| check_token("policy_version", pv, 64))
        .transpose()?;
    check_optional_fields(&OptionalRunFields {
        worktree_branch: req.worktree_branch.as_deref(),
        cwd: req.cwd.as_deref(),
        title: req.title.as_deref(),
        attach_hint: req.attach_hint.as_deref(),
        ..OptionalRunFields::default()
    })?;
    req.conflict_keys = check_conflict_keys(req.conflict_keys)?;
    let initial_state = req.state.unwrap_or(match req.origin {
        RunOrigin::Managed => RunState::Leased,
        RunOrigin::Interactive => RunState::Running,
        RunOrigin::Unknown => unreachable!("validated above"),
    });
    if initial_state == RunState::Queued
        && (req.lease_ttl_secs.is_some()
            || req.lease_expires_at.is_some()
            || !req.conflict_keys.is_empty())
    {
        return Err(invalid(
            "a queued run cannot carry a lease or conflict_keys",
            "omit lease_ttl_secs, lease_expires_at and conflict_keys until the run transitions to leased",
        ));
    }
    Ok(req)
}

/// Validates `GET /v1/runs` query filters.
pub(crate) fn validate_filter(filter: RunFilter) -> Result<RunFilter> {
    if filter.kind == Some(RunKind::Unknown) {
        return Err(invalid(
            "unknown run kind filter",
            "send kind as worker, review, verdict, triage, goal, merge, housekeeping, command or interactive",
        ));
    }
    if filter.origin == Some(RunOrigin::Unknown) {
        return Err(invalid(
            "unknown run origin filter",
            "send origin as managed or interactive",
        ));
    }
    if filter.state == Some(RunState::Unknown) {
        return Err(invalid(
            "unknown run state filter",
            "send state as queued, leased, starting, running, waiting_external, succeeded, failed, preempted, orphaned or cancelled",
        ));
    }
    Ok(filter)
}

/// Validates `PATCH /v1/runs/{id}` at the API boundary.
pub(crate) fn validate_update(mut req: UpdateRun) -> Result<UpdateRun> {
    if req.state == Some(RunState::Unknown) {
        return Err(invalid(
            "unknown run state",
            "send state as queued, leased, starting, running, waiting_external, succeeded, failed, preempted, orphaned or cancelled",
        ));
    }
    check_lease_ttl(req.lease_ttl_secs, req.lease_expires_at)?;
    if let Some(reason) = &req.reason {
        check_text("reason", reason, RUN_REASON_CHARS_MAX)?;
    }
    if let Some(pv) = req.policy_version.take() {
        req.policy_version = Some(
            pv.as_deref()
                .map(|s| check_token("policy_version", s, 64))
                .transpose()?,
        );
    }
    if let Some(sha) = req.transcript_sha256.take() {
        req.transcript_sha256 = Some(
            sha.as_deref()
                .map(|s| check_token("transcript_sha256", s, 128))
                .transpose()?,
        );
    }
    check_optional_fields(&OptionalRunFields {
        worktree_branch: req.worktree_branch.as_ref().and_then(|o| o.as_deref()),
        cwd: req.cwd.as_ref().and_then(|o| o.as_deref()),
        title: req.title.as_ref().and_then(|o| o.as_deref()),
        attach_hint: req.attach_hint.as_ref().and_then(|o| o.as_deref()),
        transcript_uri: req.transcript_uri.as_ref().and_then(|o| o.as_deref()),
        outcome: req.outcome.as_ref().and_then(|o| o.as_ref()),
    })?;
    req.conflict_keys = check_conflict_keys(req.conflict_keys)?;
    Ok(req)
}

pub(crate) fn new_run_id(cx: &Ctx<'_>) -> Result<RunId> {
    for _ in 0..8 {
        let candidate: RunId = format!("run-{}", random_hex::<12>())
            .parse()
            .expect("run-<hex> is a valid RunId");
        if !cx.exists("SELECT 1 FROM runs WHERE id = ?1", [candidate.as_str()])? {
            return Ok(candidate);
        }
    }
    unreachable!("96-bit random run ids do not collide")
}

/// Atomically acquires `specs` in `conflict_locks` for `run_id`, refusing with `409
/// claim_refused` if any key is already at capacity. Skips keys already held by `run_id`.
pub(crate) fn acquire_conflict_locks(
    cx: &Ctx<'_>,
    run_id: &RunId,
    specs: &[ConflictKeySpec],
) -> Result<Vec<String>> {
    let now = cx.now_ms();
    let mut acquired = Vec::new();
    for spec in specs {
        let existing: Vec<(String, u32)> = cx
            .db
            .prepare_cached(
                "SELECT run_id, max_holders FROM conflict_locks WHERE key = ?1 \
                 ORDER BY acquired_at, run_id",
            )?
            .query_map([&spec.key], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if existing.iter().any(|(holder, _)| holder == run_id.as_str()) {
            continue;
        }
        let effective_max = existing.iter().map(|(_, m)| *m).fold(spec.max, u32::min);
        if existing.len() >= effective_max as usize {
            let holders: Vec<&str> = existing.iter().map(|(h, _)| h.as_str()).collect();
            return Err(refused(
                format!(
                    "conflict lock {:?} is at capacity ({}/{effective_max}, held by {})",
                    spec.key,
                    existing.len(),
                    holders.join(", ")
                ),
                format!(
                    "wait for {} to reach a terminal state (which releases {:?}) or pick another task",
                    holders.join(", "),
                    spec.key
                ),
            ));
        }
        cx.db.execute(
            "INSERT INTO conflict_locks (key, run_id, max_holders, acquired_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![spec.key, run_id.as_str(), spec.max, now],
        )?;
        acquired.push(spec.key.clone());
    }
    Ok(acquired)
}

/// Releases every conflict lock held by `run_id` and returns the released keys in order.
fn release_conflict_locks(cx: &Ctx<'_>, run_id: &RunId) -> Result<Vec<String>> {
    let keys: Vec<String> = cx
        .db
        .prepare_cached("SELECT key FROM conflict_locks WHERE run_id = ?1 ORDER BY key")?
        .query_map([run_id.as_str()], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if !keys.is_empty() {
        cx.db.execute(
            "DELETE FROM conflict_locks WHERE run_id = ?1",
            [run_id.as_str()],
        )?;
    }
    Ok(keys)
}

/// Loads one run by id with its currently held `conflict_locks`.
pub(crate) fn get(cx: &Ctx<'_>, id: &RunId) -> Result<Run> {
    let run = cx
        .db
        .prepare_cached(&format!("SELECT {RUN_COLUMNS} FROM runs r WHERE r.id = ?1"))?
        .query_row([id.as_str()], run_row)
        .optional()?
        .ok_or_else(|| {
            not_found(
                format!("run {id} does not exist"),
                "GET /v1/runs lists the runs",
            )
        })?;
    Ok(complete_runs(cx, vec![run])?
        .into_iter()
        .next()
        .expect("one run in, one run out"))
}

/// Lists runs matching `filter`, newest first (`ORDER BY created_at DESC, id DESC`).
pub(crate) fn list(cx: &Ctx<'_>, filter: RunFilter) -> Result<Vec<Run>> {
    let mut clauses = vec!["1 = 1"];
    let mut params: NamedParams<'_> = Vec::new();
    let board = filter.board.as_ref().map(|b| b.as_str().to_owned());
    let task = filter.task.as_ref().map(|t| t.as_str().to_owned());
    let device = filter.device.as_ref().map(|d| d.as_str().to_owned());
    let agent = filter
        .agent
        .as_ref()
        .map(|a| agent_by_id_or_handle(cx, a))
        .transpose()?
        .map(|a| a.as_str().to_owned());
    let kind = filter.kind.map(|k| enum_str(&k));
    let origin = filter.origin.map(|o| enum_str(&o));
    let state = filter.state.map(|s| enum_str(&s));
    let limit = i64::from(
        filter
            .limit
            .unwrap_or(RUNS_LIMIT_DEFAULT)
            .clamp(1, RUNS_LIMIT_MAX),
    );

    if let Some(ref board) = board {
        clauses.push("r.board = :board");
        params.push((":board", board));
    }
    if let Some(ref task) = task {
        clauses.push("r.task_id = :task");
        params.push((":task", task));
    }
    if let Some(ref device) = device {
        clauses.push("r.device = :device");
        params.push((":device", device));
    }
    if let Some(ref agent) = agent {
        clauses.push("r.agent_id = :agent");
        params.push((":agent", agent));
    }
    if let Some(ref kind) = kind {
        clauses.push("r.kind = :kind");
        params.push((":kind", kind));
    }
    if let Some(ref origin) = origin {
        clauses.push("r.origin = :origin");
        params.push((":origin", origin));
    }
    if let Some(ref state) = state {
        clauses.push("r.state = :state");
        params.push((":state", state));
    }
    match filter.active {
        Some(true) => {
            clauses
                .push("r.state IN ('queued', 'leased', 'starting', 'running', 'waiting_external')");
        }
        Some(false) => {
            clauses
                .push("r.state IN ('succeeded', 'failed', 'preempted', 'orphaned', 'cancelled')");
        }
        None => {}
    }
    params.push((":limit", &limit));

    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM runs r WHERE {} ORDER BY r.created_at DESC, r.id DESC LIMIT :limit",
        clauses.join(" AND ")
    );
    let runs = cx
        .db
        .prepare_cached(&sql)?
        .query_map(params.as_slice(), run_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    complete_runs(cx, runs)
}

/// Creates a new `Run` (`POST /v1/runs`), acquiring any requested `conflict_keys` atomically
/// and recording a `run_created` event.
pub(crate) fn create(cx: &mut Ctx<'_>, actor: &str, req: CreateRun) -> Result<Run> {
    if !cx.exists(
        "SELECT 1 FROM devices WHERE name = ?1 AND revoked_at IS NULL",
        [req.device.as_str()],
    )? {
        return Err(not_found(
            format!("no active device named {}", req.device),
            "GET /v1/devices lists the active devices",
        ));
    }
    let board = match (&req.task_id, &req.board) {
        (Some(task_id), maybe_board) => {
            let task = get_task(cx, task_id)?;
            if let Some(board) = maybe_board
                && *board != task.board
            {
                return Err(invalid(
                    format!("task {task_id} is on board {}, not {board}", task.board),
                    format!(
                        "send board={} or leave board out to derive it from {task_id}",
                        task.board
                    ),
                ));
            }
            Some(task.board)
        }
        (None, Some(board)) => {
            get_board_uncounted(cx, board)?;
            Some(board.clone())
        }
        (None, None) => None,
    };
    let agent_id = req
        .agent_id
        .as_ref()
        .map(|a| agent_by_id_or_handle(cx, a))
        .transpose()?;
    let state = req.state.unwrap_or(match req.origin {
        RunOrigin::Managed => RunState::Leased,
        RunOrigin::Interactive => RunState::Running,
        RunOrigin::Unknown => unreachable!("validated at boundary"),
    });
    let lease_expires_at: Option<i64> = if state == RunState::Queued {
        None
    } else if let Some(at) = req.lease_expires_at {
        Some(check_lease_expiry(cx.now, at)?)
    } else if let Some(ttl) = req.lease_ttl_secs {
        Some(expires(cx, i64::try_from(ttl * 1000).unwrap_or(i64::MAX)))
    } else if req.origin == RunOrigin::Managed {
        let policy = crate::ops::policy::active(cx)?;
        Some(expires(
            cx,
            crate::ops::policy::effective_lease_ms(cx, &policy),
        ))
    } else {
        None
    };

    let now = cx.now_ms();
    let started_at = (state == RunState::Running).then_some(now);
    let id = new_run_id(cx)?;

    cx.db.execute(
        "INSERT INTO runs ( \
             id, task_id, board, kind, origin, device, runner, agent_id, state, \
             lease_expires_at, worktree_branch, cwd, title, attach_hint, cost_units, \
             policy_version, created_at, started_at, ended_at, updated_at \
         ) VALUES ( \
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, \
             ?10, ?11, ?12, ?13, ?14, ?15, \
             ?16, ?17, ?18, NULL, ?17 \
         )",
        params![
            id.as_str(),
            req.task_id.as_ref().map(TaskId::as_str),
            board.as_ref().map(BoardSlug::as_str),
            enum_str(&req.kind),
            enum_str(&req.origin),
            req.device.as_str(),
            req.runner,
            agent_id.as_ref().map(AgentId::as_str),
            enum_str(&state),
            lease_expires_at,
            req.worktree_branch,
            req.cwd,
            req.title,
            req.attach_hint,
            req.cost_units,
            req.policy_version,
            now,
            started_at,
        ],
    )?;

    let acquired = acquire_conflict_locks(cx, &id, &req.conflict_keys)?;
    if req.origin == RunOrigin::Managed
        && !state.is_terminal()
        && state != RunState::Queued
        && let Some(ref agent) = agent_id
    {
        absorb_auto_interactive_runs(cx, &id, agent)?;
    }
    let mut payload = serde_json::json!({
        "run_id": id,
        "kind": req.kind,
        "origin": req.origin,
        "device": req.device,
        "runner": req.runner,
        "state": state,
        "cost_units": req.cost_units,
    });
    if let Some(ref agent) = agent_id {
        payload["agent_id"] = serde_json::json!(agent);
    }
    if let Some(at) = lease_expires_at {
        payload["lease_expires_at"] = serde_json::json!(ts(at));
    }
    if !acquired.is_empty() {
        payload["conflict_keys"] = serde_json::json!(acquired);
    }
    cx.emit(
        board.as_ref(),
        actor,
        EventType::RunCreated,
        req.task_id.as_ref(),
        payload,
    )?;
    get(cx, &id)
}

/// Updates a run's state, lease expiry, or metadata (`PATCH /v1/runs/{id}`).
/// Enforces device ownership (device-self or user), strict Run state machine transitions,
/// atomic `conflict_locks` release on terminal states, and Trail Rule event emission.
pub(crate) fn update(
    cx: &mut Ctx<'_>,
    id: &RunId,
    actor: &str,
    caller_device: Option<&DeviceName>,
    caller_agent: Option<&AgentId>,
    by_user: bool,
    req: UpdateRun,
) -> Result<Run> {
    let before = get(cx, id)?;
    if !by_user && caller_device != Some(&before.device) {
        return Err(forbidden(
            format!("run {id} belongs to device {}", before.device),
            format!(
                "only device {} or the user may update run {id}",
                before.device
            ),
        ));
    }
    if !by_user
        && before.origin == RunOrigin::Managed
        && let Some(agent) = caller_agent
        && agent.dispatch_device().as_ref() != Some(&before.device)
    {
        return Err(forbidden(
            format!(
                "agent {agent} cannot update managed run {id} on device {}",
                before.device
            ),
            format!(
                "only the user or device {}'s daemon (dispatch:{}) may update managed runs",
                before.device, before.device
            ),
        ));
    }

    if let Some(Some(ref agent)) = req.agent_id
        && let Some(device) = agent.dispatch_device()
        && device != before.device
    {
        return Err(invalid(
            format!(
                "{agent} is the dispatch agent of device {device}, not of {}",
                before.device
            ),
            format!("name device {}'s own dispatch agent", before.device),
        ));
    }

    if before.state.is_terminal() || before.state == RunState::Unknown {
        if let Some(target) = req.state {
            return Err(bad_transition(
                if before.state == RunState::Unknown {
                    format!(
                        "run {id} is in an unknown state from a newer server and cannot transition"
                    )
                } else {
                    format!(
                        "run {id} is already in terminal state {} and cannot transition to {}",
                        state_str(before.state),
                        state_str(target)
                    )
                },
                if before.state == RunState::Unknown {
                    "upgrade the server before transitioning this run"
                } else {
                    "terminal runs cannot transition; create a new run instead"
                },
            ));
        }
        if req.lease_ttl_secs.is_some() || req.lease_expires_at.is_some() {
            return Err(bad_transition(
                format!(
                    "cannot renew lease on run {id} in state {}",
                    state_str(before.state)
                ),
                "omit lease_ttl_secs and lease_expires_at on terminal or unknown-state runs",
            ));
        }
        if !req.conflict_keys.is_empty() {
            return Err(bad_transition(
                format!(
                    "cannot acquire conflict locks on run {id} in state {}",
                    state_str(before.state)
                ),
                "terminal or unknown-state runs cannot acquire conflict locks",
            ));
        }
    }

    let (next_state, state_changed) = match req.state {
        Some(target) if target != before.state => {
            if !before.state.can_transition_to(target) {
                let allowed = allowed_targets(before.state);
                let hint = if allowed.is_empty() {
                    "terminal runs cannot transition".to_owned()
                } else {
                    format!(
                        "allowed transitions from {}: {}",
                        state_str(before.state),
                        allowed.join(", ")
                    )
                };
                return Err(bad_transition(
                    format!(
                        "cannot transition run {id} from {} to {}",
                        state_str(before.state),
                        state_str(target)
                    ),
                    hint,
                ));
            }
            (target, true)
        }
        _ => (before.state, false),
    };

    if next_state == RunState::Queued
        && (req.lease_ttl_secs.is_some()
            || req.lease_expires_at.is_some()
            || !req.conflict_keys.is_empty())
    {
        return Err(invalid(
            "a queued run cannot carry a lease or conflict_keys",
            "transition the run to leased before setting a lease or acquiring conflict_keys",
        ));
    }
    if next_state.is_terminal() && (req.lease_ttl_secs.is_some() || req.lease_expires_at.is_some())
    {
        return Err(invalid(
            "cannot set a lease while transitioning to a terminal state",
            "omit lease_ttl_secs and lease_expires_at when ending a run",
        ));
    }
    if next_state.is_terminal() && !req.conflict_keys.is_empty() {
        return Err(invalid(
            "cannot acquire conflict_keys while transitioning to a terminal state",
            "omit conflict_keys when ending a run",
        ));
    }

    let now = cx.now_ms();
    let new_lease_expires_at: Option<i64> = if next_state.is_terminal() {
        None
    } else if let Some(at) = req.lease_expires_at {
        Some(check_lease_expiry(cx.now, at)?)
    } else if let Some(ttl) = req.lease_ttl_secs {
        Some(expires(cx, i64::try_from(ttl * 1000).unwrap_or(i64::MAX)))
    } else if before.state == RunState::Queued
        && next_state == RunState::Leased
        && before.origin == RunOrigin::Managed
    {
        let policy = crate::ops::policy::active(cx)?;
        Some(expires(
            cx,
            crate::ops::policy::effective_lease_ms(cx, &policy),
        ))
    } else {
        before.lease_expires_at.map(|t| t.timestamp_millis())
    };
    let lease_changed =
        new_lease_expires_at != before.lease_expires_at.map(|t| t.timestamp_millis());

    let started_at: Option<i64> = match before.started_at {
        Some(at) => Some(at.timestamp_millis()),
        None if next_state == RunState::Running => Some(now),
        None => None,
    };
    let ended_at: Option<i64> = match before.ended_at {
        Some(at) => Some(at.timestamp_millis()),
        None if next_state.is_terminal() => Some(now),
        None => None,
    };

    let mut fields: Vec<&str> = Vec::new();
    if state_changed {
        fields.push("state");
    }

    let agent_id = match req.agent_id {
        Some(Some(ref a)) => {
            let resolved = agent_by_id_or_handle(cx, a)?;
            if before.agent_id.as_ref() != Some(&resolved) {
                fields.push("agent_id");
            }
            Some(resolved)
        }
        Some(None) => {
            if before.agent_id.is_some() {
                fields.push("agent_id");
            }
            None
        }
        None => before.agent_id.clone(),
    };
    let worktree_branch = match req.worktree_branch {
        Some(v) => {
            if v != before.worktree_branch {
                fields.push("worktree_branch");
            }
            v
        }
        None => before.worktree_branch.clone(),
    };
    let cwd = match req.cwd {
        Some(v) => {
            if v != before.cwd {
                fields.push("cwd");
            }
            v
        }
        None => before.cwd.clone(),
    };
    let title = match req.title {
        Some(v) => {
            if v != before.title {
                fields.push("title");
            }
            v
        }
        None => before.title.clone(),
    };
    let attach_hint = match req.attach_hint {
        Some(v) => {
            if v != before.attach_hint {
                fields.push("attach_hint");
            }
            v
        }
        None => before.attach_hint.clone(),
    };
    let policy_version = match req.policy_version {
        Some(v) => {
            if v != before.policy_version {
                fields.push("policy_version");
            }
            v
        }
        None => before.policy_version.clone(),
    };
    let outcome = match req.outcome {
        Some(v) => {
            if v != before.outcome {
                fields.push("outcome");
            }
            v
        }
        None => before.outcome.clone(),
    };
    let transcript_uri = match req.transcript_uri {
        Some(v) => {
            if v != before.transcript_uri {
                fields.push("transcript_uri");
            }
            v
        }
        None => before.transcript_uri.clone(),
    };
    let transcript_sha256 = match req.transcript_sha256 {
        Some(v) => {
            if v != before.transcript_sha256 {
                fields.push("transcript_sha256");
            }
            v
        }
        None => before.transcript_sha256.clone(),
    };

    let acquired_locks = if !next_state.is_terminal() && !req.conflict_keys.is_empty() {
        acquire_conflict_locks(cx, id, &req.conflict_keys)?
    } else {
        Vec::new()
    };
    if !acquired_locks.is_empty() {
        fields.push("conflict_keys");
    }

    let (released_locks, expired_wakes) = if state_changed && next_state.is_terminal() {
        (
            release_conflict_locks(cx, id)?,
            expire_run_wakes(cx, id, now)?,
        )
    } else {
        (Vec::new(), Vec::new())
    };

    if fields.is_empty() && !lease_changed {
        return Ok(before);
    }

    let outcome_json = outcome
        .as_ref()
        .map(|v| serde_json::to_string(v).expect("outcome serializes"));
    cx.db.execute(
        "UPDATE runs SET \
             state = CASE WHEN ?1 THEN ?2 ELSE state END, \
             lease_expires_at = ?3, \
             agent_id = ?4, \
             worktree_branch = ?5, \
             cwd = ?6, \
             title = ?7, \
             attach_hint = ?8, \
             policy_version = ?9, \
             outcome_json = ?10, \
             transcript_uri = ?11, \
             transcript_sha256 = ?12, \
             started_at = ?13, \
             ended_at = ?14, \
             updated_at = ?15 \
         WHERE id = ?16",
        params![
            state_changed,
            enum_str(&next_state),
            new_lease_expires_at,
            agent_id.as_ref().map(AgentId::as_str),
            worktree_branch,
            cwd,
            title,
            attach_hint,
            policy_version,
            outcome_json,
            transcript_uri,
            transcript_sha256,
            started_at,
            ended_at,
            now,
            id.as_str(),
        ],
    )?;

    if before.origin == RunOrigin::Managed
        && !next_state.is_terminal()
        && next_state != RunState::Queued
        && let Some(ref agent) = agent_id
    {
        absorb_auto_interactive_runs(cx, id, agent)?;
    }

    if (state_changed && next_state.is_terminal())
        || (before.state.is_terminal() && fields.contains(&"outcome"))
    {
        record_run_outcome(cx, id)?;
    }

    // Trail Rule: emit a durable event for state transitions and meaningful metadata changes,
    // without spamming the event log on pure 30s lease-renewal heartbeats.
    if !fields.is_empty() {
        let event_type = if state_changed && next_state == RunState::Cancelled {
            EventType::RunCancelled
        } else if state_changed && next_state == RunState::Orphaned {
            EventType::RunOrphaned
        } else {
            EventType::RunUpdated
        };
        let mut payload = serde_json::json!({
            "run_id": id,
            "kind": before.kind,
            "device": before.device,
            "fields": fields,
        });
        if state_changed {
            payload["from"] = serde_json::json!(before.state);
            payload["to"] = serde_json::json!(next_state);
        }
        if let Some(ref reason) = req.reason {
            payload["reason"] = serde_json::json!(reason);
        }
        if let Some(ref branch) = worktree_branch
            && matches!(next_state, RunState::Orphaned | RunState::Cancelled)
        {
            payload["worktree_branch"] = serde_json::json!(branch);
        }
        if state_changed
            && next_state == RunState::Orphaned
            && let Some(prev_exp) = before.lease_expires_at
        {
            payload["lease_expires_at"] = serde_json::json!(prev_exp);
        }
        if !acquired_locks.is_empty() {
            payload["acquired_locks"] = serde_json::json!(acquired_locks);
        }
        if !released_locks.is_empty() {
            payload["released_locks"] = serde_json::json!(released_locks);
        }
        if !expired_wakes.is_empty() {
            payload["expired_wakes"] = serde_json::json!(expired_wakes);
        }
        cx.emit(
            before.board.as_ref(),
            actor,
            event_type,
            before.task_id.as_ref(),
            payload,
        )?;
    }

    get(cx, id)
}

/// Sweeps managed runs whose renewable lease has expired (`lease_expires_at <= now`) while
/// in `leased | starting | running | waiting_external`, managed `waiting_external` runs
/// whose hosting device went stale beyond `sticky_grace_secs`, and pending `sticky_wakes`
/// targeting orphaned/terminal runs or devices stale beyond `sticky_grace_secs`.
///
/// Each transitioned run is marked `orphaned`, releases its `conflict_locks`, expires any
/// pending `sticky_wakes`, and emits a Trail Rule `run_orphaned` event. Standalone expired
/// sticky wakes emit a Trail Rule `run_updated` fallback event carrying the wake payload.
pub(crate) fn sweep_expired(cx: &mut Ctx<'_>) -> Result<usize> {
    let now = cx.now_ms();
    let sticky_grace_ms = crate::ops::policy::effective_sticky_grace_ms(cx)?;
    let sticky_cutoff = now.saturating_sub(sticky_grace_ms);

    // 1. Managed live runs whose renewable lease expired (`lease_expires_at <= now`).
    let expired: Vec<Run> = cx
        .db
        .prepare_cached(&format!(
            "SELECT {RUN_COLUMNS} FROM runs r \
             WHERE r.origin = 'managed' \
               AND r.state IN ('leased', 'starting', 'running', 'waiting_external') \
               AND r.lease_expires_at IS NOT NULL \
               AND r.lease_expires_at <= ?1 \
             ORDER BY r.lease_expires_at, r.id"
        ))?
        .query_map([now], run_row)?
        .collect::<rusqlite::Result<_>>()?;
    for run in &expired {
        let released = release_conflict_locks(cx, &run.id)?;
        let expired_wakes = expire_run_wakes(cx, &run.id, now)?;
        cx.db.execute(
            "UPDATE runs SET state = 'orphaned', lease_expires_at = NULL, \
             ended_at = COALESCE(ended_at, ?2), updated_at = ?2 WHERE id = ?1",
            params![run.id.as_str(), now],
        )?;
        record_run_outcome(cx, &run.id)?;
        let expired_at = run.lease_expires_at.expect("selected with IS NOT NULL");
        let expired_by_ms = now.saturating_sub(expired_at.timestamp_millis());
        let mut payload = serde_json::json!({
            "run_id": run.id,
            "kind": run.kind,
            "from": run.state,
            "to": RunState::Orphaned,
            "device": run.device,
            "runner": run.runner,
            "cause": "lease_expired",
            "lease_expires_at": expired_at,
            "expired_by_ms": expired_by_ms,
        });
        if let Some(ref agent) = run.agent_id {
            payload["agent_id"] = serde_json::json!(agent);
        }
        if let Some(ref branch) = run.worktree_branch {
            payload["worktree_branch"] = serde_json::json!(branch);
        }
        if !released.is_empty() {
            payload["released_locks"] = serde_json::json!(released);
        }
        if !expired_wakes.is_empty() {
            payload["expired_wakes"] = serde_json::json!(expired_wakes);
        }
        cx.emit(
            run.board.as_ref(),
            "server",
            EventType::RunOrphaned,
            run.task_id.as_ref(),
            payload,
        )?;
    }

    // 2. Managed `waiting_external` runs whose hosting device was revoked or whose heartbeat
    //    (falling back to `created_at` if never heartbeated) is stale beyond `sticky_grace_secs`:
    //    orphan them and expire their sticky wakes so another awake device can pick up recovery.
    //    Runs paused on quota (`agents.state IN ('paused', 'ended') AND paused_until > now`)
    //    stay intact while their device is merely asleep/stale (not revoked).
    struct StaleWaiting {
        run: Run,
        heartbeat_at: Option<i64>,
        device_created_at: i64,
        device_revoked_at: Option<i64>,
    }
    let stale_waiting: Vec<StaleWaiting> = cx
        .db
        .prepare_cached(&format!(
            "SELECT {RUN_COLUMNS}, d.heartbeat_at, d.created_at, d.revoked_at FROM runs r \
             JOIN devices d ON d.name = r.device \
             LEFT JOIN agents ag ON ag.id = r.agent_id \
             WHERE r.origin = 'managed' \
               AND r.state = 'waiting_external' \
               AND ( \
                   d.revoked_at IS NOT NULL \
                   OR ( \
                       COALESCE(d.heartbeat_at, d.created_at) <= ?1 \
                       AND NOT ( \
                           ag.id IS NOT NULL \
                           AND ag.state IN ('paused', 'ended') \
                           AND ag.paused_until IS NOT NULL \
                           AND ag.paused_until > ?2 \
                       ) \
                   ) \
               ) \
             ORDER BY COALESCE(d.heartbeat_at, d.created_at), r.id"
        ))?
        .query_map(params![sticky_cutoff, now], |r| {
            Ok(StaleWaiting {
                run: run_row(r)?,
                heartbeat_at: r.get(23)?,
                device_created_at: r.get(24)?,
                device_revoked_at: r.get(25)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for StaleWaiting {
        run,
        heartbeat_at,
        device_created_at,
        device_revoked_at,
    } in &stale_waiting
    {
        let released = release_conflict_locks(cx, &run.id)?;
        let expired_wakes = expire_run_wakes(cx, &run.id, now)?;
        cx.db.execute(
            "UPDATE runs SET state = 'orphaned', lease_expires_at = NULL, \
             ended_at = COALESCE(ended_at, ?2), updated_at = ?2 WHERE id = ?1",
            params![run.id.as_str(), now],
        )?;
        record_run_outcome(cx, &run.id)?;
        let last_seen_ms = heartbeat_at.unwrap_or(*device_created_at);
        let stale_by_ms = now.saturating_sub(last_seen_ms);
        let cause = if device_revoked_at.is_some() {
            "device_revoked_sticky_fallback"
        } else {
            "device_stale_sticky_fallback"
        };
        let mut payload = serde_json::json!({
            "run_id": run.id,
            "kind": run.kind,
            "from": run.state,
            "to": RunState::Orphaned,
            "device": run.device,
            "runner": run.runner,
            "cause": cause,
            "stale_by_ms": stale_by_ms,
        });
        if let Some(hb) = heartbeat_at {
            payload["heartbeat_at"] = serde_json::json!(ts(*hb));
        }
        if let Some(rev) = device_revoked_at {
            payload["revoked_at"] = serde_json::json!(ts(*rev));
        }
        if let Some(ref agent) = run.agent_id {
            payload["agent_id"] = serde_json::json!(agent);
        }
        if let Some(ref branch) = run.worktree_branch {
            payload["worktree_branch"] = serde_json::json!(branch);
        }
        if !released.is_empty() {
            payload["released_locks"] = serde_json::json!(released);
        }
        if !expired_wakes.is_empty() {
            payload["expired_wakes"] = serde_json::json!(expired_wakes);
        }
        cx.emit(
            run.board.as_ref(),
            "server",
            EventType::RunOrphaned,
            run.task_id.as_ref(),
            payload,
        )?;
    }

    // 3. Pending sticky wakes whose run is already terminal, whose device is revoked/missing,
    //    or whose device's heartbeat (falling back to `created_at`) is stale beyond
    //    `sticky_grace_secs` (unless the target run is waiting on a quota reset): mark expired
    //    with a Trail Rule fallback event.
    struct FallbackWake {
        wake: StickyWake,
        board: Option<BoardSlug>,
        task_id: Option<TaskId>,
        run_state: Option<String>,
        heartbeat_at: Option<i64>,
        device_revoked_at: Option<i64>,
    }
    let fallback_wakes: Vec<FallbackWake> = cx
        .db
        .prepare_cached(
            "SELECT sw.id, sw.run_id, sw.agent_id, sw.device, sw.kind, sw.payload_json, \
                    sw.created_at, sw.delivered_at, sw.expires_at, \
                    r.board, r.task_id, r.state, d.heartbeat_at, d.revoked_at \
             FROM sticky_wakes sw \
             LEFT JOIN runs r ON r.id = sw.run_id \
             LEFT JOIN devices d ON d.name = sw.device \
             LEFT JOIN agents ag ON ag.id = COALESCE(sw.agent_id, r.agent_id) \
             WHERE sw.delivered_at IS NULL \
               AND (sw.expires_at IS NULL OR sw.expires_at > ?1) \
               AND ( \
                   r.state IN ('succeeded', 'failed', 'preempted', 'orphaned', 'cancelled') \
                   OR d.name IS NULL \
                   OR d.revoked_at IS NOT NULL \
                   OR ( \
                       COALESCE(d.heartbeat_at, d.created_at) <= ?2 \
                       AND NOT ( \
                           r.state = 'waiting_external' \
                           AND ag.id IS NOT NULL \
                           AND ag.state IN ('paused', 'ended') \
                           AND ag.paused_until IS NOT NULL \
                           AND ag.paused_until > ?1 \
                       ) \
                   ) \
               ) \
             ORDER BY sw.created_at, sw.id",
        )?
        .query_map(params![now, sticky_cutoff], |r| {
            Ok(FallbackWake {
                wake: sticky_wake_row(r)?,
                board: r
                    .get::<_, Option<String>>(9)?
                    .map(|s| parse_id(&s))
                    .transpose()?,
                task_id: r
                    .get::<_, Option<String>>(10)?
                    .map(|s| parse_id(&s))
                    .transpose()?,
                run_state: r.get::<_, Option<String>>(11)?,
                heartbeat_at: r.get::<_, Option<i64>>(12)?,
                device_revoked_at: r.get::<_, Option<i64>>(13)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for FallbackWake {
        wake,
        board,
        task_id,
        run_state,
        heartbeat_at,
        device_revoked_at,
    } in &fallback_wakes
    {
        cx.db.execute(
            "UPDATE sticky_wakes SET expires_at = ?2 WHERE id = ?1",
            params![wake.id, now],
        )?;
        let cause = if run_state.as_deref().is_some_and(|s| {
            matches!(
                s,
                "succeeded" | "failed" | "preempted" | "orphaned" | "cancelled"
            )
        }) {
            "sticky_wake_run_terminal"
        } else if device_revoked_at.is_some() {
            "sticky_wake_device_revoked"
        } else {
            "sticky_wake_device_stale"
        };
        let mut payload = serde_json::json!({
            "wake_id": wake.id,
            "device": wake.device,
            "wake_kind": wake.kind,
            "wake_payload": wake.payload,
            "cause": cause,
            "expired_at": ts(now),
        });
        if let Some(rev) = device_revoked_at {
            payload["revoked_at"] = serde_json::json!(ts(*rev));
        }
        if let Some(ref run_id) = wake.run_id {
            payload["run_id"] = serde_json::json!(run_id);
        }
        if let Some(ref agent_id) = wake.agent_id {
            payload["agent_id"] = serde_json::json!(agent_id);
        }
        if let Some(hb) = heartbeat_at {
            payload["heartbeat_at"] = serde_json::json!(ts(*hb));
            payload["stale_by_ms"] = serde_json::json!(now.saturating_sub(*hb));
        }
        cx.emit(
            board.as_ref(),
            "server",
            EventType::RunUpdated,
            task_id.as_ref(),
            payload,
        )?;
    }

    Ok(expired.len() + stale_waiting.len() + fallback_wakes.len())
}

// ---------------------------------------------------------------------------------------
// Sticky wakes

const STICKY_WAKE_COLUMNS: &str = "id, run_id, agent_id, device, kind, payload_json, \
     created_at, delivered_at, expires_at";

fn sticky_wake_row(r: &Row<'_>) -> rusqlite::Result<StickyWake> {
    Ok(StickyWake {
        id: r.get(0)?,
        run_id: r
            .get::<_, Option<String>>(1)?
            .map(|s| parse_id(&s))
            .transpose()?,
        agent_id: r
            .get::<_, Option<String>>(2)?
            .map(|s| parse_id(&s))
            .transpose()?,
        device: parse_id(&r.get::<_, String>(3)?)?,
        kind: r.get(4)?,
        payload: parse_json(&r.get::<_, String>(5)?)?,
        created_at: ts(r.get(6)?),
        delivered_at: r.get::<_, Option<i64>>(7)?.map(ts),
        expires_at: r.get::<_, Option<i64>>(8)?.map(ts),
    })
}

fn expire_run_wakes(cx: &Ctx<'_>, run_id: &RunId, now: i64) -> Result<Vec<i64>> {
    let ids: Vec<i64> = cx
        .db
        .prepare_cached(
            "SELECT id FROM sticky_wakes \
             WHERE run_id = ?1 AND delivered_at IS NULL \
               AND (expires_at IS NULL OR expires_at > ?2) \
             ORDER BY id",
        )?
        .query_map(params![run_id.as_str(), now], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if !ids.is_empty() {
        cx.db.execute(
            "UPDATE sticky_wakes SET expires_at = ?2 \
             WHERE run_id = ?1 AND delivered_at IS NULL \
               AND (expires_at IS NULL OR expires_at > ?2)",
            params![run_id.as_str(), now],
        )?;
    }
    Ok(ids)
}

/// Routes a follow-up wake into `sticky_wakes` when `agent` currently has a live managed
/// run (`starting | running | waiting_external` with unexpired lease).
pub(crate) fn enqueue_sticky_wake_for_agent(
    cx: &Ctx<'_>,
    agent: &AgentId,
    kind: &str,
    payload: serde_json::Value,
) -> Result<Option<i64>> {
    let now = cx.now_ms();
    let target: Option<(String, String)> = cx
        .db
        .prepare_cached(
            "SELECT id, device FROM runs \
             WHERE agent_id = ?1 AND origin = 'managed' \
               AND state IN ('starting', 'running', 'waiting_external') \
               AND (lease_expires_at IS NULL OR lease_expires_at > ?2) \
             ORDER BY updated_at DESC, id DESC LIMIT 1",
        )?
        .query_row(params![agent.as_str(), now], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((run_id, device)) = target else {
        return Ok(None);
    };
    cx.db.execute(
        "INSERT INTO sticky_wakes (run_id, agent_id, device, kind, payload_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            run_id,
            agent.as_str(),
            device,
            kind,
            payload.to_string(),
            now
        ],
    )?;
    Ok(Some(cx.db.last_insert_rowid()))
}

/// Lists undelivered, unexpired sticky wakes for `device` whose target run (if any) is still
/// non-terminal, oldest first (`GET /v1/devices/{name}/wakes`).
pub(crate) fn list_device_wakes(cx: &Ctx<'_>, device: &DeviceName) -> Result<Vec<StickyWake>> {
    if !cx.exists(
        "SELECT 1 FROM devices WHERE name = ?1 AND revoked_at IS NULL",
        [device.as_str()],
    )? {
        return Err(not_found(
            format!("no active device named {device}"),
            "GET /v1/devices lists the active devices",
        ));
    }
    let now = cx.now_ms();
    Ok(cx
        .db
        .prepare_cached(
            "SELECT sw.id, sw.run_id, sw.agent_id, sw.device, sw.kind, sw.payload_json, \
                    sw.created_at, sw.delivered_at, sw.expires_at \
             FROM sticky_wakes sw \
             LEFT JOIN runs r ON r.id = sw.run_id \
             WHERE sw.device = ?1 AND sw.delivered_at IS NULL \
               AND (sw.expires_at IS NULL OR sw.expires_at > ?2) \
               AND (r.id IS NULL OR r.state NOT IN ('succeeded', 'failed', 'preempted', 'orphaned', 'cancelled')) \
             ORDER BY sw.created_at, sw.id",
        )?
        .query_map(params![device.as_str(), now], sticky_wake_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Marks sticky wake `id` delivered (`POST /v1/wakes/{id}/ack`). Authorized for the wake's
/// owning device daemon (no `X-Board-Agent` or `dispatch:<device>`) or a user token;
/// idempotent once delivered.
pub(crate) fn ack_wake(
    cx: &mut Ctx<'_>,
    id: i64,
    caller_device: Option<&DeviceName>,
    caller_agent: Option<&AgentId>,
    by_user: bool,
) -> Result<StickyWake> {
    let wake = cx
        .db
        .prepare_cached(&format!(
            "SELECT {STICKY_WAKE_COLUMNS} FROM sticky_wakes WHERE id = ?1"
        ))?
        .query_row([id], sticky_wake_row)
        .optional()?
        .ok_or_else(|| {
            not_found(
                format!("sticky wake {id} does not exist"),
                "GET /v1/devices/{name}/wakes lists pending wakes for your device",
            )
        })?;
    if !by_user {
        if caller_device != Some(&wake.device) {
            return Err(forbidden(
                format!("sticky wake {id} belongs to device {}", wake.device),
                format!(
                    "only device {} or the user may acknowledge sticky wake {id}",
                    wake.device
                ),
            ));
        }
        if let Some(agent) = caller_agent
            && agent.dispatch_device().as_ref() != Some(&wake.device)
        {
            return Err(forbidden(
                format!(
                    "agent {agent} cannot acknowledge sticky wake {id} on device {}",
                    wake.device
                ),
                format!(
                    "only the user or device {}'s daemon (dispatch:{}) may acknowledge sticky wakes",
                    wake.device, wake.device
                ),
            ));
        }
    }
    if wake.delivered_at.is_none() {
        cx.db.execute(
            "UPDATE sticky_wakes SET delivered_at = ?2 WHERE id = ?1 AND delivered_at IS NULL",
            params![id, cx.now_ms()],
        )?;
    }
    Ok(cx
        .db
        .prepare_cached(&format!(
            "SELECT {STICKY_WAKE_COLUMNS} FROM sticky_wakes WHERE id = ?1"
        ))?
        .query_row([id], sticky_wake_row)?)
}

// ---------------------------------------------------------------------------------------
// Dispatcher-agnostic standing `review: pass` verification

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewVerdict {
    Pass,
    Fail,
    Void,
}

/// Parses a `review:` decision note verdict, matching `crates/board/src/daemon/judges.rs`.
fn review_verdict(text: &str) -> Option<ReviewVerdict> {
    let mut rest = text.trim_start();
    for kind in ["decision:", "finding:", "note:"] {
        if let Some(after) = rest.strip_prefix(kind) {
            rest = after.trim_start();
            break;
        }
    }
    let after = rest.strip_prefix("review:")?.trim_start().to_lowercase();
    if after.starts_with("pass") {
        Some(ReviewVerdict::Pass)
    } else if after.starts_with("fail") {
        Some(ReviewVerdict::Fail)
    } else if after.starts_with("void") {
        Some(ReviewVerdict::Void)
    } else {
        None
    }
}

/// Legacy dispatcher note prefix written when starting a review judge thread.
const LEGACY_JUDGE_NOTE_PREFIX: &str = "note: the dispatcher started a review judge for this task:";

/// Whether `agent` was registered as a managed review runner or legacy review judge for
/// `task_id` on `board` (on any device).
fn is_registered_reviewer(
    cx: &Ctx<'_>,
    board: &BoardSlug,
    task_id: &TaskId,
    agent: &AgentId,
) -> Result<bool> {
    // 1. Registered managed `review` Run for this task (any device) that was actually
    //    leased/started (excluding `queued` and `queued -> cancelled` runs).
    let by_run: bool = cx
        .db
        .prepare_cached(
            "SELECT EXISTS ( \
                 SELECT 1 FROM runs r \
                 WHERE r.task_id = ?1 AND r.kind = 'review' AND r.origin = 'managed' \
                   AND r.agent_id = ?2 \
                   AND r.state != 'queued' \
                   AND (r.state != 'cancelled' OR r.started_at IS NOT NULL) \
             )",
        )?
        .query_row(params![task_id.as_str(), agent.as_str()], |r| r.get(0))?;
    if by_run {
        return Ok(true);
    }

    // 2. Legacy registered review judge (live row in `judges`, or durable `board_updated`
    //    judge-registration event via `events_board_judge` partial index, or exact-match
    //    legacy dispatcher start note — never SQLite LIKE against untrusted agent ids).
    let suffix = format!("(agent {agent})");
    let by_legacy_judge: bool = cx
        .db
        .prepare_cached(
            "SELECT EXISTS ( \
                 SELECT 1 FROM judges j \
                 WHERE j.board = ?1 AND j.agent = ?2 AND j.kind = 'review' \
                   AND EXISTS (SELECT 1 FROM json_each(j.tasks) jt WHERE jt.value = ?3) \
             ) OR EXISTS ( \
                 SELECT 1 FROM events e \
                 WHERE e.board = ?1 AND e.type = 'board_updated' \
                   AND json_extract(e.payload, '$.judge') = ?2 \
                   AND json_extract(e.payload, '$.kind') = 'review' \
                   AND EXISTS ( \
                       SELECT 1 FROM json_each(json_extract(e.payload, '$.tasks')) jt \
                       WHERE jt.value = ?3 \
                   ) \
             ) OR EXISTS ( \
                 SELECT 1 FROM notes n \
                 WHERE n.task = ?3 AND substr(n.agent, 1, 9) = 'dispatch:' \
                   AND substr(n.text, 1, length(?4)) = ?4 \
                   AND length(n.text) >= length(?4) + length(?5) \
                   AND substr(n.text, -length(?5)) = ?5 \
             )",
        )?
        .query_row(
            params![
                board.as_str(),
                agent.as_str(),
                task_id.as_str(),
                LEGACY_JUDGE_NOTE_PREFIX,
                suffix,
            ],
            |r| r.get(0),
        )?;
    Ok(by_legacy_judge)
}

/// Checks whether `task_id` currently has a standing `review: pass` decision note
/// (`docs/SCHEDULER_DESIGN.md`, Section 3.3).
///
/// Returns `Ok(Some(note))` if and only if:
/// 1. The task is currently in `review`.
/// 2. Its newest `decision` note carrying a `review:` verdict was written **after** the
///    task's latest move to `review`.
/// 3. That verdict is `review: pass` (a `review: fail` or `review: void`—even from a run
///    that exited `succeeded`—never counts as a standing pass).
/// 4. The note's author is not the task's owner and matches either a registered `review`
///    [`Run`] for `task_id` (on any device) or a legacy registered review judge.
pub(crate) fn standing_review_pass(cx: &Ctx<'_>, task_id: &TaskId) -> Result<Option<Note>> {
    let task = get_task(cx, task_id)?;
    if task.status != TaskStatus::Review {
        return Ok(None);
    }
    let latest_review: Option<(i64, i64)> = cx
        .db
        .prepare_cached(
            "SELECT seq, at FROM events \
             WHERE task = ?1 AND type = 'task_updated' \
               AND ( \
                   ( \
                       json_extract(payload, '$.to') = 'review' \
                       AND NOT ( \
                           json_extract(payload, '$.from') = 'blocked' \
                           AND substr(actor, 1, 9) = 'dispatch:' \
                       ) \
                   ) \
                   OR json_extract(payload, '$.to') IN ('doing', 'open') \
               ) \
             ORDER BY seq DESC LIMIT 1",
        )?
        .query_row([task_id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;

    let decision_notes: Vec<(Note, Option<i64>)> = cx
        .db
        .prepare_cached(
            "SELECT n.id, n.agent, n.kind, n.text, n.at, \
                    (SELECT e.seq FROM events e \
                     WHERE e.task = n.task AND e.type = 'note_added' \
                       AND json_extract(e.payload, '$.note') = n.id \
                     ORDER BY e.seq DESC LIMIT 1) AS note_seq \
             FROM notes n \
             WHERE n.task = ?1 AND n.kind = 'decision' \
             ORDER BY n.id DESC",
        )?
        .query_map([task_id.as_str()], |r| {
            Ok((
                Note {
                    id: r.get(0)?,
                    task: task_id.clone(),
                    agent: parse_id(&r.get::<_, String>(1)?)?,
                    kind: parse_enum(&r.get::<_, String>(2)?)?,
                    text: r.get(3)?,
                    at: ts(r.get(4)?),
                },
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let Some((note, note_seq, verdict)) = decision_notes.into_iter().find_map(|(note, seq)| {
        let verdict = review_verdict(&note.text)?;
        Some((note, seq, verdict))
    }) else {
        return Ok(None);
    };

    if let Some((review_seq, review_at)) = latest_review {
        let after_review = match note_seq {
            Some(seq) => seq > review_seq,
            None => note.at.timestamp_millis() > review_at,
        };
        if !after_review {
            return Ok(None);
        }
    }

    if verdict != ReviewVerdict::Pass {
        return Ok(None);
    }
    if task.owner.as_ref() == Some(&note.agent) {
        return Ok(None);
    }
    if !is_registered_reviewer(cx, &task.board, task_id, &note.agent)? {
        return Ok(None);
    }
    Ok(Some(note))
}

// ---------------------------------------------------------------------------------------
// Interactive session unification (`docs/SCHEDULER_DESIGN.md`, Section 5.1)

fn interactive_runner(agent: &Agent) -> String {
    if agent.id.source() == AgentSource::T3
        || agent.native_ids.contains_key("t3_link")
        || agent.native_ids.contains_key("t3_thread")
    {
        "t3".to_owned()
    } else {
        enum_str(&agent.runtime)
    }
}

fn interactive_attach_hint(agent: &Agent) -> Option<String> {
    if let Some(link) = agent.native_ids.get("t3_link") {
        return Some(link.clone());
    }
    let cwd = agent.cwd.as_deref().filter(|c| !c.is_empty())?;
    let ssh = if cwd.starts_with('/') {
        format!("ssh://{}{cwd}", agent.device)
    } else {
        format!("ssh://{}/{cwd}", agent.device)
    };
    if ssh.chars().count() <= ATTACH_HINT_CHARS_MAX {
        Some(ssh)
    } else if cwd.chars().count() <= ATTACH_HINT_CHARS_MAX {
        Some(cwd.to_owned())
    } else {
        None
    }
}

fn doing_task_for_agent(cx: &Ctx<'_>, agent: &AgentId) -> Result<Option<(TaskId, BoardSlug)>> {
    cx.db
        .prepare_cached(
            "SELECT id, board FROM tasks \
             WHERE owner = ?1 AND status = 'doing' \
             ORDER BY doing_since DESC, id DESC LIMIT 1",
        )?
        .query_row([agent.as_str()], |r| {
            Ok((
                parse_id::<TaskId>(&r.get::<_, String>(0)?)?,
                parse_id::<BoardSlug>(&r.get::<_, String>(1)?)?,
            ))
        })
        .optional()
        .map_err(Into::into)
}

fn primary_board_for_agent(
    cx: &Ctx<'_>,
    agent: &Agent,
    doing_board: Option<&BoardSlug>,
) -> Result<Option<BoardSlug>> {
    if let Some(board) = doing_board {
        return Ok(Some(board.clone()));
    }
    if let Some(repo) = agent.repo.as_deref() {
        let boards = crate::ops::agents::boards_for_repo(cx, repo)?;
        if boards.len() == 1 {
            return Ok(Some(boards.into_iter().next().expect("checked len == 1")));
        }
    }
    let joined: Vec<BoardSlug> = cx
        .db
        .prepare_cached(
            "SELECT b.slug FROM board_members m \
             JOIN boards b ON b.slug = m.board \
             WHERE m.agent = ?1 AND b.status = 'active' \
             ORDER BY b.slug",
        )?
        .query_map([agent.id.as_str()], |r| parse_id(&r.get::<_, String>(0)?))?
        .collect::<rusqlite::Result<_>>()?;
    if joined.len() == 1 {
        Ok(Some(joined.into_iter().next().expect("checked len == 1")))
    } else {
        Ok(None)
    }
}

fn active_managed_run_for_agent(cx: &Ctx<'_>, agent: &AgentId) -> Result<Option<RunId>> {
    cx.db
        .prepare_cached(
            "SELECT id FROM runs \
             WHERE agent_id = ?1 AND origin = 'managed' \
               AND state IN ('queued', 'leased', 'starting', 'running', 'waiting_external') \
             ORDER BY updated_at DESC, id DESC LIMIT 1",
        )?
        .query_row([agent.as_str()], |r| parse_id(&r.get::<_, String>(0)?))
        .optional()
        .map_err(Into::into)
}

fn active_interactive_run_for_agent(cx: &Ctx<'_>, agent: &AgentId) -> Result<Option<RunId>> {
    cx.db
        .prepare_cached(
            "SELECT id FROM runs \
             WHERE agent_id = ?1 AND origin = 'interactive' \
               AND state IN ('queued', 'leased', 'starting', 'running', 'waiting_external') \
             ORDER BY updated_at DESC, id DESC LIMIT 1",
        )?
        .query_row([agent.as_str()], |r| parse_id(&r.get::<_, String>(0)?))
        .optional()
        .map_err(Into::into)
}

fn next_interactive_run_id(cx: &Ctx<'_>, agent: &AgentId) -> Result<RunId> {
    let digest = hex(&Sha256::digest(agent.as_str().as_bytes()));
    let base = format!("{INTERACTIVE_RUN_PREFIX}{}", &digest[..12]);
    if !cx.exists("SELECT 1 FROM runs WHERE id = ?1", [&base])? {
        return Ok(base
            .parse()
            .expect("run-interactive-<hex> is a valid RunId"));
    }
    for n in 2u32..=1000 {
        let candidate = format!("{base}-{n}");
        if !cx.exists("SELECT 1 FROM runs WHERE id = ?1", [&candidate])? {
            return Ok(candidate
                .parse()
                .expect("run-interactive-<hex>-<n> is a valid RunId"));
        }
    }
    for _ in 0..8 {
        let candidate = format!("{INTERACTIVE_RUN_PREFIX}{}", random_hex::<12>());
        if !cx.exists("SELECT 1 FROM runs WHERE id = ?1", [&candidate])? {
            return Ok(candidate
                .parse()
                .expect("run-interactive-<hex> is a valid RunId"));
        }
    }
    unreachable!("96-bit random interactive run ids do not collide")
}

/// When a live managed run is created or updated with `agent_id`, absorbs any active
/// auto-upserted interactive placeholder run (`run-interactive-*`) that raced ahead during
/// `SessionStart`, preserving any metadata it captured without leaving a duplicate run.
pub(crate) fn absorb_auto_interactive_runs(
    cx: &Ctx<'_>,
    managed_id: &RunId,
    agent: &AgentId,
) -> Result<()> {
    struct InteractivePlaceholder {
        id: String,
        branch: Option<String>,
        cwd: Option<String>,
        title: Option<String>,
        attach_hint: Option<String>,
    }
    let placeholders: Vec<InteractivePlaceholder> = cx
        .db
        .prepare_cached(
            "SELECT id, worktree_branch, cwd, title, attach_hint FROM runs \
             WHERE agent_id = ?1 \
               AND origin = 'interactive' \
               AND kind = 'interactive' \
               AND id LIKE 'run-interactive-%' \
               AND state IN ('queued', 'leased', 'starting', 'running', 'waiting_external') \
             ORDER BY updated_at DESC, id DESC",
        )?
        .query_map([agent.as_str()], |r| {
            Ok(InteractivePlaceholder {
                id: r.get(0)?,
                branch: r.get(1)?,
                cwd: r.get(2)?,
                title: r.get(3)?,
                attach_hint: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let Some(newest) = placeholders.first() else {
        return Ok(());
    };
    cx.db.execute(
        "UPDATE runs SET \
             worktree_branch = COALESCE(worktree_branch, ?2), \
             cwd = COALESCE(cwd, ?3), \
             title = COALESCE(title, ?4), \
             attach_hint = COALESCE(attach_hint, ?5) \
         WHERE id = ?1",
        params![
            managed_id.as_str(),
            newest.branch,
            newest.cwd,
            newest.title,
            newest.attach_hint,
        ],
    )?;
    for placeholder in &placeholders {
        cx.db
            .execute("DELETE FROM runs WHERE id = ?1", [placeholder.id.as_str()])?;
    }
    Ok(())
}

/// Upserts an active `origin = 'interactive'` `Run` for `agent` inside `POST /v1/agents`,
/// or syncs session metadata onto an existing active `origin = 'managed'` `Run` without
/// creating a duplicate. Skips `dispatch:<device>` agents.
pub(crate) fn upsert_for_registered_agent(cx: &mut Ctx<'_>, agent: &Agent) -> Result<()> {
    if agent.id.source() == AgentSource::Dispatch {
        return Ok(());
    }
    let now = cx.now_ms();
    let doing = doing_task_for_agent(cx, &agent.id)?;
    let attach_hint = interactive_attach_hint(agent);

    if let Some(managed_id) = active_managed_run_for_agent(cx, &agent.id)? {
        let doing_task = doing.as_ref().map(|(t, _)| t.as_str());
        let doing_board = doing.as_ref().map(|(_, b)| b.as_str());
        // Only an explicit `t3_link` overrides an existing managed `attach_hint`; a synthesized
        // `ssh://` fallback only fills a missing `attach_hint` so we never clobber a managed
        // runner's explicit attach URI.
        let explicit_attach = agent.native_ids.get("t3_link").map(String::as_str);
        cx.db.execute(
            "UPDATE runs SET \
                 worktree_branch = COALESCE(?2, worktree_branch), \
                 cwd = COALESCE(?3, cwd), \
                 title = COALESCE(?4, title), \
                 attach_hint = COALESCE(?5, attach_hint, ?6), \
                 task_id = COALESCE(task_id, ?7), \
                 board = COALESCE(board, ?8), \
                 updated_at = ?9 \
             WHERE id = ?1 \
               AND ( \
                   COALESCE(?2, worktree_branch) IS NOT worktree_branch \
                   OR COALESCE(?3, cwd) IS NOT cwd \
                   OR COALESCE(?4, title) IS NOT title \
                   OR COALESCE(?5, attach_hint, ?6) IS NOT attach_hint \
                   OR COALESCE(task_id, ?7) IS NOT task_id \
                   OR COALESCE(board, ?8) IS NOT board \
               )",
            params![
                managed_id.as_str(),
                agent.branch.as_deref(),
                agent.cwd.as_deref(),
                agent.title.as_deref(),
                explicit_attach,
                attach_hint.as_deref(),
                doing_task,
                doing_board,
                now,
            ],
        )?;
        return Ok(());
    }

    // `runs.device` references `devices(name)`. Skip silently if a user token registered an
    // agent with a synthetic device name not present in `devices`.
    if !cx.exists(
        "SELECT 1 FROM devices WHERE name = ?1",
        [agent.device.as_str()],
    )? {
        return Ok(());
    }

    let board = primary_board_for_agent(cx, agent, doing.as_ref().map(|(_, b)| b))?;
    let task_id = doing.as_ref().map(|(t, _)| t);
    let runner = interactive_runner(agent);

    if let Some(run_id) = active_interactive_run_for_agent(cx, &agent.id)? {
        cx.db.execute(
            "UPDATE runs SET \
                 task_id = ?2, \
                 board = ?3, \
                 device = ?4, \
                 runner = ?5, \
                 state = 'running', \
                 worktree_branch = ?6, \
                 cwd = ?7, \
                 title = COALESCE(?8, title), \
                 attach_hint = ?9, \
                 started_at = COALESCE(started_at, ?10), \
                 ended_at = NULL, \
                 updated_at = ?10 \
             WHERE id = ?1",
            params![
                run_id.as_str(),
                task_id.map(TaskId::as_str),
                board.as_ref().map(BoardSlug::as_str),
                agent.device.as_str(),
                runner,
                agent.branch.as_deref(),
                agent.cwd.as_deref(),
                agent.title.as_deref(),
                attach_hint.as_deref(),
                now,
            ],
        )?;
        return Ok(());
    }

    let id = next_interactive_run_id(cx, &agent.id)?;
    cx.db.execute(
        "INSERT INTO runs ( \
             id, task_id, board, kind, origin, device, runner, agent_id, state, \
             lease_expires_at, worktree_branch, cwd, title, attach_hint, cost_units, \
             policy_version, created_at, started_at, ended_at, updated_at \
         ) VALUES ( \
             ?1, ?2, ?3, 'interactive', 'interactive', ?4, ?5, ?6, 'running', \
             NULL, ?7, ?8, ?9, ?10, ?11, \
             NULL, ?12, ?12, NULL, ?12 \
         )",
        params![
            id.as_str(),
            task_id.map(TaskId::as_str),
            board.as_ref().map(BoardSlug::as_str),
            agent.device.as_str(),
            runner,
            agent.id.as_str(),
            agent.branch.as_deref(),
            agent.cwd.as_deref(),
            agent.title.as_deref(),
            attach_hint.as_deref(),
            INTERACTIVE_RUN_COST_UNITS,
            now,
        ],
    )?;
    Ok(())
}

/// Syncs an agent's active `origin = 'interactive'` `Run` with its hook lifecycle state:
/// - `Running` → `RunState::Running` (and creates a fresh interactive run if none is active
///   and the agent has no active managed run)
/// - `Idle` / `Paused` → `RunState::WaitingExternal`
/// - `Ended` → `RunState::Succeeded` (`ended_at = now`)
pub(crate) fn sync_for_agent_state(
    cx: &mut Ctx<'_>,
    id: &AgentId,
    state: AgentState,
) -> Result<()> {
    if id.source() == AgentSource::Dispatch {
        return Ok(());
    }
    if state == AgentState::Ended {
        return end_interactive_for_agent(cx, id);
    }
    if active_managed_run_for_agent(cx, id)?.is_some() {
        return Ok(());
    }
    let now = cx.now_ms();
    match state {
        AgentState::Running => {
            if let Some(run_id) = active_interactive_run_for_agent(cx, id)? {
                let doing = doing_task_for_agent(cx, id)?;
                let doing_task = doing.as_ref().map(|(t, _)| t.as_str());
                let doing_board = doing.as_ref().map(|(_, b)| b.as_str());
                cx.db.execute(
                    "UPDATE runs SET \
                         state = 'running', \
                         task_id = ?2, \
                         board = COALESCE(?3, board), \
                         started_at = COALESCE(started_at, ?4), \
                         updated_at = ?4 \
                     WHERE id = ?1",
                    params![run_id.as_str(), doing_task, doing_board, now],
                )?;
            } else {
                let agent = crate::store::get_agent(cx, id)?;
                upsert_for_registered_agent(cx, &agent)?;
            }
        }
        AgentState::Idle | AgentState::Paused => {
            if let Some(run_id) = active_interactive_run_for_agent(cx, id)? {
                let doing = doing_task_for_agent(cx, id)?;
                let doing_task = doing.as_ref().map(|(t, _)| t.as_str());
                let doing_board = doing.as_ref().map(|(_, b)| b.as_str());
                cx.db.execute(
                    "UPDATE runs SET \
                         state = 'waiting_external', \
                         task_id = ?2, \
                         board = COALESCE(?3, board), \
                         updated_at = ?4 \
                     WHERE id = ?1",
                    params![run_id.as_str(), doing_task, doing_board, now],
                )?;
            }
        }
        AgentState::Ended => unreachable!("handled above"),
    }
    Ok(())
}

/// Closes every active `origin = 'interactive'` `Run` for `id` (`state = 'succeeded'`,
/// `ended_at = now`), releasing any held `conflict_locks` and expiring any pending
/// `sticky_wakes`. Called on explicit `SessionEnd` and by `end_unseen`.
pub(crate) fn end_interactive_for_agent(cx: &mut Ctx<'_>, id: &AgentId) -> Result<()> {
    if id.source() == AgentSource::Dispatch {
        return Ok(());
    }
    let now = cx.now_ms();
    let active_ids: Vec<RunId> = cx
        .db
        .prepare_cached(
            "SELECT id FROM runs \
             WHERE agent_id = ?1 \
               AND origin = 'interactive' \
               AND state IN ('queued', 'leased', 'starting', 'running', 'waiting_external') \
             ORDER BY id",
        )?
        .query_map([id.as_str()], |r| parse_id(&r.get::<_, String>(0)?))?
        .collect::<rusqlite::Result<_>>()?;
    for run_id in &active_ids {
        release_conflict_locks(cx, run_id)?;
        expire_run_wakes(cx, run_id, now)?;
        cx.db.execute(
            "UPDATE runs SET \
                 state = 'succeeded', \
                 ended_at = COALESCE(ended_at, ?2), \
                 updated_at = ?2 \
             WHERE id = ?1",
            params![run_id.as_str(), now],
        )?;
        record_run_outcome(cx, run_id)?;
    }
    Ok(())
}

/// Atomically computes and upserts the `run_outcomes` telemetry row for a terminal `Run`
/// (`docs/SCHEDULER_DESIGN.md`, Section 6.1).
pub(crate) fn record_run_outcome(cx: &Ctx<'_>, run_id: &RunId) -> Result<()> {
    let run = get(cx, run_id)?;
    if !run.state.is_terminal() {
        return Ok(());
    }
    let created_ms = run.created_at.timestamp_millis();
    let started_ms = run.started_at.map(|t| t.timestamp_millis());
    let ended_ms = run
        .ended_at
        .map(|t| t.timestamp_millis())
        .unwrap_or_else(|| cx.now_ms());

    let ready_ms = match &run.task_id {
        Some(tid) => last_ready_ms_for_run(cx, tid, run.kind, created_ms, Some(&run.id))?,
        None => None,
    };
    let ready_to_leased_secs = ready_ms.map(|r| (created_ms - r).max(0) / 1000);
    let leased_to_running_secs = started_ms.map(|s| (s - created_ms).max(0) / 1000);
    let running_secs = started_ms.map(|s| (ended_ms - s).max(0) / 1000);
    let weight_unit_minutes =
        running_secs.map(|secs| f64::from(run.cost_units) * (secs as f64) / 60.0);

    let (orphan_count, retry_count): (i64, i64) = if let Some(ref tid) = run.task_id {
        cx.db.query_row(
            "SELECT \
                 COALESCE(SUM(CASE WHEN state = 'orphaned' THEN 1 ELSE 0 END), 0), \
                 COALESCE(SUM(CASE WHEN state IN ('orphaned', 'failed') THEN 1 ELSE 0 END), 0) \
             FROM runs \
             WHERE task_id = ?1 AND kind = ?2 AND id != ?3 \
               AND (created_at < ?4 OR (created_at = ?4 AND id < ?3))",
            params![
                tid.as_str(),
                enum_str(&run.kind),
                run.id.as_str(),
                created_ms
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
    } else {
        (0, 0)
    };

    let outcome_obj = run.outcome.as_ref().and_then(serde_json::Value::as_object);
    let opt_str = |k: &str| -> Option<String> {
        outcome_obj
            .and_then(|m| m.get(k))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let opt_nonneg_i64 = |k: &str| -> Option<i64> {
        outcome_obj
            .and_then(|m| m.get(k))
            .and_then(serde_json::Value::as_i64)
            .filter(|&v| v >= 0)
    };
    let opt_bool_i64 = |k: &str| -> Option<i64> {
        outcome_obj
            .and_then(|m| m.get(k))
            .and_then(serde_json::Value::as_bool)
            .map(i64::from)
    };

    let quota_bucket = opt_str("quota_bucket");
    let tokens_in = opt_nonneg_i64("tokens_in");
    let tokens_out = opt_nonneg_i64("tokens_out");
    let warm_cache_hit = opt_bool_i64("warm_cache_hit");
    let ci_runs_before_merge = opt_nonneg_i64("ci_runs_before_merge");
    let review_rounds = opt_nonneg_i64("review_rounds");
    let first_pass_merge = opt_bool_i64("first_pass_merge");
    let mut conflict_wait_secs = opt_nonneg_i64("conflict_wait_secs");
    if conflict_wait_secs.is_none()
        && let Some(ref tid) = run.task_id
    {
        let window_start = ready_ms.unwrap_or_else(|| created_ms.saturating_sub(24 * 3_600_000));
        let earliest_hold: Option<i64> = cx
            .db
            .prepare_cached(
                "SELECT MIN(at) FROM scheduler_holds \
                 WHERE task_id = ?1 AND reason = 'conflict_key' \
                   AND at >= ?2 AND at <= ?3",
            )?
            .query_row(params![tid.as_str(), window_start, created_ms], |r| {
                r.get(0)
            })?;
        if let Some(hold_at) = earliest_hold {
            conflict_wait_secs = Some((created_ms - hold_at).max(0) / 1000);
        }
    }

    cx.db.execute(
        "INSERT INTO run_outcomes ( \
             run_id, board, task_id, device, kind, terminal_state, \
             ready_to_leased_secs, leased_to_running_secs, running_secs, conflict_wait_secs, \
             weight_unit_minutes, quota_bucket, tokens_in, tokens_out, warm_cache_hit, \
             orphan_count, retry_count, ci_runs_before_merge, review_rounds, first_pass_merge, \
             policy_version, recorded_at \
         ) VALUES ( \
             ?1, ?2, ?3, ?4, ?5, ?6, \
             ?7, ?8, ?9, ?10, \
             ?11, ?12, ?13, ?14, ?15, \
             ?16, ?17, ?18, ?19, ?20, \
             ?21, ?22 \
         ) ON CONFLICT(run_id) DO UPDATE SET \
             board = excluded.board, \
             task_id = excluded.task_id, \
             device = excluded.device, \
             kind = excluded.kind, \
             terminal_state = excluded.terminal_state, \
             ready_to_leased_secs = excluded.ready_to_leased_secs, \
             leased_to_running_secs = excluded.leased_to_running_secs, \
             running_secs = excluded.running_secs, \
             conflict_wait_secs = excluded.conflict_wait_secs, \
             weight_unit_minutes = excluded.weight_unit_minutes, \
             quota_bucket = excluded.quota_bucket, \
             tokens_in = excluded.tokens_in, \
             tokens_out = excluded.tokens_out, \
             warm_cache_hit = excluded.warm_cache_hit, \
             orphan_count = excluded.orphan_count, \
             retry_count = excluded.retry_count, \
             ci_runs_before_merge = excluded.ci_runs_before_merge, \
             review_rounds = excluded.review_rounds, \
             first_pass_merge = excluded.first_pass_merge, \
             policy_version = excluded.policy_version, \
             recorded_at = excluded.recorded_at",
        params![
            run.id.as_str(),
            run.board.as_ref().map(BoardSlug::as_str),
            run.task_id.as_ref().map(TaskId::as_str),
            run.device.as_str(),
            enum_str(&run.kind),
            enum_str(&run.state),
            ready_to_leased_secs,
            leased_to_running_secs,
            running_secs,
            conflict_wait_secs,
            weight_unit_minutes,
            quota_bucket,
            tokens_in,
            tokens_out,
            warm_cache_hit,
            orphan_count,
            retry_count,
            ci_runs_before_merge,
            review_rounds,
            first_pass_merge,
            run.policy_version.as_deref(),
            ended_ms,
        ],
    )?;
    Ok(())
}

/// Computes the latest time `task_id` became ready for `kind` at or before `leased_ms`.
pub(crate) fn last_ready_ms_for_run(
    cx: &Ctx<'_>,
    task_id: &TaskId,
    kind: RunKind,
    leased_ms: i64,
    exclude_run_id: Option<&RunId>,
) -> Result<Option<i64>> {
    let row: Option<(i64, Option<i64>)> = cx
        .db
        .prepare_cached("SELECT created_at, not_before FROM tasks WHERE id = ?1")?
        .query_row([task_id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((created_at, not_before)) = row else {
        return Ok(None);
    };
    let ready = match kind {
        RunKind::Review => {
            let entered_review: Option<i64> = cx
                .db
                .prepare_cached(
                    "SELECT MAX(at) FROM events \
                     WHERE task = ?1 AND type = 'task_updated' \
                       AND json_extract(payload, '$.to') = 'review' \
                       AND at <= ?2",
                )?
                .query_row(params![task_id.as_str(), leased_ms], |r| r.get(0))?;
            entered_review.unwrap_or(created_at)
        }
        RunKind::Merge => {
            let pass_at: Option<i64> = cx
                .db
                .prepare_cached(
                    "SELECT MAX(at) FROM notes \
                     WHERE task = ?1 AND kind = 'decision' AND at <= ?2",
                )?
                .query_row(params![task_id.as_str(), leased_ms], |r| r.get(0))?;
            pass_at.unwrap_or(created_at)
        }
        RunKind::Worker => {
            let reset: Option<i64> = cx
                .db
                .prepare_cached(
                    "SELECT MAX(at) FROM events \
                     WHERE task = ?1 AND at <= ?2 \
                       AND (type = 'task_released' \
                            OR (type = 'task_updated' AND json_extract(payload, '$.to') = 'open'))",
                )?
                .query_row(params![task_id.as_str(), leased_ms], |r| r.get(0))?;
            let blocker_done: Option<i64> = cx
                .db
                .prepare_cached(
                    "SELECT MAX(e.at) FROM task_blockers b \
                     JOIN events e ON e.task = b.blocker AND e.type = 'task_done' \
                     WHERE b.task = ?1 AND e.at <= ?2",
                )?
                .query_row(params![task_id.as_str(), leased_ms], |r| r.get(0))?;
            let prior_ended: Option<i64> = cx
                .db
                .prepare_cached(
                    "SELECT MAX(COALESCE(ended_at, updated_at)) FROM runs \
                     WHERE task_id = ?1 AND kind = 'worker' \
                       AND state IN ('orphaned', 'failed', 'preempted', 'cancelled') \
                       AND COALESCE(ended_at, updated_at) <= ?2 \
                       AND (?3 IS NULL OR id != ?3)",
                )?
                .query_row(
                    params![
                        task_id.as_str(),
                        leased_ms,
                        exclude_run_id.map(RunId::as_str)
                    ],
                    |r| r.get(0),
                )?;
            let nb = not_before.filter(|&t| t <= leased_ms);
            [Some(created_at), reset, blocker_done, prior_ended, nb]
                .into_iter()
                .flatten()
                .max()
                .unwrap_or(created_at)
        }
        _ => created_at,
    };
    Ok(Some(ready.min(leased_ms)))
}

/// Keeps an active `origin = 'interactive'` `Run`'s `task_id` (and `board`) in sync with
/// `agent`'s current `doing` task whenever tasks are claimed, updated, released, or done.
pub(crate) fn sync_agent_doing_task(cx: &Ctx<'_>, agent: &AgentId) -> Result<()> {
    if agent.source() == AgentSource::Dispatch {
        return Ok(());
    }
    let Some(run_id) = active_interactive_run_for_agent(cx, agent)? else {
        return Ok(());
    };
    let doing = doing_task_for_agent(cx, agent)?;
    let now = cx.now_ms();
    match doing {
        Some((task_id, board)) => {
            cx.db.execute(
                "UPDATE runs SET task_id = ?2, board = ?3, updated_at = ?4 \
                 WHERE id = ?1 AND (task_id IS NOT ?2 OR board IS NOT ?3)",
                params![run_id.as_str(), task_id.as_str(), board.as_str(), now],
            )?;
        }
        None => {
            cx.db.execute(
                "UPDATE runs SET task_id = NULL, updated_at = ?2 \
                 WHERE id = ?1 AND task_id IS NOT NULL",
                params![run_id.as_str(), now],
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Live run events (`POST/GET /v1/runs/{id}/events`), steering (`POST /v1/runs/{id}/steer`),
// cancellation (`POST /v1/runs/{id}/cancel`), and retention pruning
// (`docs/SCHEDULER_DESIGN.md`, Sections 5.2 & 5.3)

fn run_event_row(r: &Row<'_>) -> rusqlite::Result<RunEventRecord> {
    Ok(RunEventRecord {
        run_id: parse_id(&r.get::<_, String>(0)?)?,
        seq: r.get(1)?,
        at: ts(r.get(2)?),
        kind: parse_enum(&r.get::<_, String>(3)?)?,
        summary: r.get(4)?,
        meta: r
            .get::<_, Option<String>>(5)?
            .map(|s| parse_json(&s))
            .transpose()?,
    })
}

/// Validates `POST /v1/runs/{id}/events` at the API boundary.
pub(crate) fn validate_push_events(req: PushRunEvents) -> Result<PushRunEvents> {
    if req.events.len() > RUN_EVENTS_BATCH_MAX {
        return Err(invalid(
            format!(
                "events batch has {} entries; the limit is {RUN_EVENTS_BATCH_MAX}",
                req.events.len()
            ),
            format!("send at most {RUN_EVENTS_BATCH_MAX} events per request"),
        ));
    }
    for (idx, ev) in req.events.iter().enumerate() {
        if ev.seq <= 0 {
            return Err(invalid(
                format!("events[{idx}].seq={} must be positive", ev.seq),
                "send positive monotonic sequence numbers starting at 1",
            ));
        }
        if ev.kind == RunEventKind::Unknown {
            return Err(invalid(
                format!("events[{idx}].kind is unknown"),
                "send kind as phase, tool, note, message, pr, error, cancelled or done",
            ));
        }
        check_line("summary", &ev.summary, RUN_EVENT_SUMMARY_CHARS_MAX)?;
        if let Some(meta) = &ev.meta {
            let len = serde_json::to_string(meta)
                .map(|s| s.len())
                .unwrap_or(usize::MAX);
            if len > RUN_EVENT_META_BYTES_MAX {
                return Err(invalid(
                    format!(
                        "events[{idx}].meta is {len} bytes; the limit is {RUN_EVENT_META_BYTES_MAX}"
                    ),
                    "keep live event metadata compact and archive large payloads in the full transcript",
                ));
            }
        }
    }
    Ok(req)
}

/// Appends a batch of structured live events to `run_events`, atomically prunes oldest rows
/// beyond [`RUN_EVENTS_CAP`] (200) per run in the same `BEGIN IMMEDIATE` transaction, and
/// queues retained newly inserted records for post-commit SSE broadcast.
pub(crate) fn push_events(
    cx: &mut Ctx<'_>,
    id: &RunId,
    caller_device: Option<&DeviceName>,
    caller_agent: Option<&AgentId>,
    by_user: bool,
    mut req: PushRunEvents,
) -> Result<Vec<RunEventRecord>> {
    let run = get(cx, id)?;
    if !by_user && caller_device != Some(&run.device) {
        return Err(forbidden(
            format!("run {id} belongs to device {}", run.device),
            format!(
                "only device {} or the user may push live events for run {id}",
                run.device
            ),
        ));
    }
    if !by_user
        && run.origin == RunOrigin::Managed
        && let Some(agent) = caller_agent
        && agent.dispatch_device().as_ref() != Some(&run.device)
        && run.agent_id.as_ref() != Some(agent)
    {
        return Err(forbidden(
            format!(
                "agent {agent} cannot push events for managed run {id} on device {}",
                run.device
            ),
            format!(
                "only the user, device {}'s daemon (dispatch:{}), or the run's own agent may push events",
                run.device, run.device
            ),
        ));
    }
    if req.events.is_empty() {
        return Ok(Vec::new());
    }

    req.events.sort_by_key(|e| e.seq);
    let mut inserted = Vec::new();
    for ev in req.events {
        let at_ts = ev.at.map(crate::clock::truncate).unwrap_or(cx.now);
        let meta_json = ev
            .meta
            .as_ref()
            .map(|v| serde_json::to_string(v).expect("run event meta serializes"));
        let changed = cx.db.execute(
            "INSERT INTO run_events (run_id, seq, at, kind, summary, meta_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(run_id, seq) DO NOTHING",
            params![
                id.as_str(),
                ev.seq,
                at_ts.timestamp_millis(),
                enum_str(&ev.kind),
                ev.summary,
                meta_json,
            ],
        )?;
        if changed > 0 {
            inserted.push(RunEventRecord {
                run_id: id.clone(),
                seq: ev.seq,
                at: at_ts,
                kind: ev.kind,
                summary: ev.summary,
                meta: ev.meta,
            });
        }
    }

    // Enforce bounded cap of latest `RUN_EVENTS_CAP` (200) events per run atomically.
    cx.db.execute(
        "DELETE FROM run_events \
         WHERE run_id = ?1 \
           AND seq NOT IN ( \
               SELECT seq FROM run_events \
               WHERE run_id = ?1 \
               ORDER BY seq DESC \
               LIMIT ?2 \
           )",
        params![id.as_str(), RUN_EVENTS_CAP as i64],
    )?;

    if !inserted.is_empty() {
        let min_kept: Option<i64> = cx.db.query_row(
            "SELECT MIN(seq) FROM run_events WHERE run_id = ?1",
            [id.as_str()],
            |r| r.get(0),
        )?;
        if let Some(min_seq) = min_kept {
            inserted.retain(|ev| ev.seq >= min_seq);
        } else {
            inserted.clear();
        }
        cx.broadcast_run_events(inserted.clone());
    }
    Ok(inserted)
}

/// Reads stored live events for `id` with `seq > after_seq`, oldest first.
pub(crate) fn list_events(
    cx: &Ctx<'_>,
    id: &RunId,
    after_seq: i64,
    limit: u32,
) -> Result<Vec<RunEventRecord>> {
    let limit = i64::from(limit.clamp(1, RUN_EVENTS_LIMIT_MAX));
    let events = cx
        .db
        .prepare_cached(
            "SELECT run_id, seq, at, kind, summary, meta_json \
             FROM run_events \
             WHERE run_id = ?1 AND seq > ?2 \
             ORDER BY seq ASC \
             LIMIT ?3",
        )?
        .query_map(params![id.as_str(), after_seq, limit], run_event_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(events)
}

/// Validates `POST /v1/runs/{id}/steer` at the API boundary.
pub(crate) fn validate_steer(req: SteerRunRequest) -> Result<SteerRunRequest> {
    if req.message.trim().is_empty() {
        return Err(invalid(
            "message is empty",
            "send a non-empty steering message",
        ));
    }
    check_text("message", &req.message, RUN_STEER_MESSAGE_CHARS_MAX)?;
    Ok(req)
}

/// Verifies caller authorization for steering or cancelling `run`: allowed for the user
/// (`--user` token or browser login), the run's owning device daemon (no `X-Board-Agent` or
/// `dispatch:<run.device>`), or (for `interactive` runs) the run's own agent on that device.
fn authorize_run_control(
    run: &Run,
    action: &str,
    caller_device: Option<&DeviceName>,
    caller_agent: Option<&AgentId>,
    by_user: bool,
) -> Result<()> {
    if by_user {
        return Ok(());
    }
    if caller_device != Some(&run.device) {
        return Err(forbidden(
            format!("run {} belongs to device {}", run.id, run.device),
            format!(
                "only the user or device {} may {action} run {}",
                run.device, run.id
            ),
        ));
    }
    if let Some(agent) = caller_agent {
        let is_own_dispatch = agent.dispatch_device().as_ref() == Some(&run.device);
        let is_own_interactive =
            run.origin == RunOrigin::Interactive && run.agent_id.as_ref() == Some(agent);
        if !is_own_dispatch && !is_own_interactive {
            return Err(forbidden(
                format!(
                    "agent {agent} cannot {action} run {} on device {}",
                    run.id, run.device
                ),
                format!(
                    "only the user or device {}'s daemon (dispatch:{}) may {action} this run",
                    run.device, run.device
                ),
            ));
        }
    }
    Ok(())
}

/// Enqueues a `kind = "steer"` sticky wake targeting the active run's device and writes a
/// Trail Rule `run_steered` event (`POST /v1/runs/{id}/steer`).
pub(crate) fn steer(
    cx: &mut Ctx<'_>,
    id: &RunId,
    actor: &str,
    caller_device: Option<&DeviceName>,
    caller_agent: Option<&AgentId>,
    by_user: bool,
    req: SteerRunRequest,
) -> Result<SteerRunResponse> {
    let run = get(cx, id)?;
    authorize_run_control(&run, "steer", caller_device, caller_agent, by_user)?;
    if !run.state.is_live() {
        return Err(bad_transition(
            format!("cannot steer run {id} in state {}", state_str(run.state)),
            "only active runs (leased, starting, running, waiting_external) can be steered",
        ));
    }
    let now = cx.now_ms();
    let wake_payload = serde_json::json!({
        "run_id": run.id,
        "board": run.board,
        "task_id": run.task_id,
        "message": req.message,
        "actor": actor,
    });
    cx.db.execute(
        "INSERT INTO sticky_wakes (run_id, agent_id, device, kind, payload_json, created_at) \
         VALUES (?1, ?2, ?3, 'steer', ?4, ?5)",
        params![
            run.id.as_str(),
            run.agent_id.as_ref().map(AgentId::as_str),
            run.device.as_str(),
            wake_payload.to_string(),
            now,
        ],
    )?;
    let wake_id = cx.db.last_insert_rowid();

    let mut event_payload = serde_json::json!({
        "run_id": run.id,
        "wake_id": wake_id,
        "kind": run.kind,
        "device": run.device,
        "state": run.state,
        "message": req.message,
    });
    if let Some(ref agent) = run.agent_id {
        event_payload["agent_id"] = serde_json::json!(agent);
    }
    cx.emit(
        run.board.as_ref(),
        actor,
        EventType::RunSteered,
        run.task_id.as_ref(),
        event_payload,
    )?;

    Ok(SteerRunResponse {
        wake_id,
        run_id: run.id,
        device: run.device,
    })
}

/// Validates `POST /v1/runs/{id}/cancel` at the API boundary.
pub(crate) fn validate_cancel(req: CancelRunRequest) -> Result<CancelRunRequest> {
    if let Some(ref reason) = req.reason {
        if reason.trim().is_empty() {
            return Err(invalid(
                "reason is empty",
                "omit reason or send a non-empty cancellation reason",
            ));
        }
        check_text("reason", reason, RUN_CANCEL_REASON_CHARS_MAX)?;
    }
    Ok(req)
}

/// Cancels a non-terminal run (`POST /v1/runs/{id}/cancel`):
/// - `queued` runs transition immediately to `cancelled` (releasing any `conflict_locks`,
///   expiring pending `sticky_wakes`, and recording `run_outcomes`);
/// - active (`leased | starting | running | waiting_external`) runs enqueue a `kind = "cancel"`
///   sticky wake targeting `run.device`.
///
/// Both paths emit a Trail Rule `run_cancelled` event carrying `actor` and `reason`.
pub(crate) fn cancel(
    cx: &mut Ctx<'_>,
    id: &RunId,
    actor: &str,
    caller_device: Option<&DeviceName>,
    caller_agent: Option<&AgentId>,
    by_user: bool,
    req: CancelRunRequest,
) -> Result<CancelRunResponse> {
    let run = get(cx, id)?;
    authorize_run_control(&run, "cancel", caller_device, caller_agent, by_user)?;
    if !run.state.is_active() {
        return Err(bad_transition(
            format!(
                "run {id} is already in state {} and cannot be cancelled",
                state_str(run.state)
            ),
            "only queued or active runs can be cancelled",
        ));
    }
    let now = cx.now_ms();
    let reason = req
        .reason
        .unwrap_or_else(|| format!("cancelled by {actor}"));

    if run.state == RunState::Queued {
        let released_locks = release_conflict_locks(cx, id)?;
        let expired_wakes = expire_run_wakes(cx, id, now)?;
        cx.db.execute(
            "UPDATE runs SET state = 'cancelled', lease_expires_at = NULL, \
             ended_at = COALESCE(ended_at, ?2), updated_at = ?2 WHERE id = ?1",
            params![id.as_str(), now],
        )?;
        record_run_outcome(cx, id)?;
        let mut payload = serde_json::json!({
            "run_id": run.id,
            "kind": run.kind,
            "device": run.device,
            "from": run.state,
            "to": RunState::Cancelled,
            "reason": reason,
        });
        if !released_locks.is_empty() {
            payload["released_locks"] = serde_json::json!(released_locks);
        }
        if !expired_wakes.is_empty() {
            payload["expired_wakes"] = serde_json::json!(expired_wakes);
        }
        cx.emit(
            run.board.as_ref(),
            actor,
            EventType::RunCancelled,
            run.task_id.as_ref(),
            payload,
        )?;
        return Ok(CancelRunResponse {
            run_id: run.id,
            device: run.device,
            state: RunState::Cancelled,
            wake_id: None,
        });
    }

    let wake_payload = serde_json::json!({
        "run_id": run.id,
        "board": run.board,
        "task_id": run.task_id,
        "reason": reason,
        "actor": actor,
    });
    cx.db.execute(
        "INSERT INTO sticky_wakes (run_id, agent_id, device, kind, payload_json, created_at) \
         VALUES (?1, ?2, ?3, 'cancel', ?4, ?5)",
        params![
            run.id.as_str(),
            run.agent_id.as_ref().map(AgentId::as_str),
            run.device.as_str(),
            wake_payload.to_string(),
            now,
        ],
    )?;
    let wake_id = cx.db.last_insert_rowid();

    let mut payload = serde_json::json!({
        "run_id": run.id,
        "wake_id": wake_id,
        "kind": run.kind,
        "device": run.device,
        "state": run.state,
        "status": "cancel_requested",
        "reason": reason,
    });
    if let Some(ref agent) = run.agent_id {
        payload["agent_id"] = serde_json::json!(agent);
    }
    if let Some(ref branch) = run.worktree_branch {
        payload["worktree_branch"] = serde_json::json!(branch);
    }
    cx.emit(
        run.board.as_ref(),
        actor,
        EventType::RunCancelled,
        run.task_id.as_ref(),
        payload,
    )?;

    Ok(CancelRunResponse {
        run_id: run.id,
        device: run.device,
        state: run.state,
        wake_id: Some(wake_id),
    })
}

/// Prunes `run_events` for terminal runs older than retention (7 days after `ended_at`), or
/// once `transcript_uri` has been recorded and the 1-hour grace period has elapsed.
pub(crate) fn prune_run_events(cx: &Ctx<'_>) -> Result<usize> {
    let now = cx.now_ms();
    let archived_cutoff = now.saturating_sub(RUN_EVENTS_ARCHIVED_GRACE_MS);
    let retention_cutoff = now.saturating_sub(RUN_EVENTS_RETENTION_MS);
    let removed = cx.db.execute(
        "DELETE FROM run_events \
         WHERE run_id IN ( \
             SELECT id FROM runs \
             WHERE state IN ('succeeded', 'failed', 'preempted', 'orphaned', 'cancelled') \
               AND ( \
                   (transcript_uri IS NOT NULL AND COALESCE(updated_at, ended_at, created_at) <= ?1) \
                   OR COALESCE(ended_at, updated_at, created_at) <= ?2 \
               ) \
         )",
        params![archived_cutoff, retention_cutoff],
    )?;
    Ok(removed)
}
