//! Atomic pull-based scheduler (`POST /v1/dispatch/claim`):
//! Kueue-style Admission, Kubernetes Filter/Taints, Slurm Multifactor Score/Fair-Share/Backfill,
//! fleet-wide `conflict_keys`, and 3-minute renewable `Run` leases inside one `BEGIN IMMEDIATE`
//! transaction (`docs/SCHEDULER_DESIGN.md`, Sections 2, 3, 6.4).

use std::collections::{BTreeMap, HashMap, HashSet};

use board_proto::{
    AgentId, Board, BoardSlug, BoardStatus, ClaimedRun, ConflictKeySpec,
    DISPATCH_MAX_CLAIMS_DEFAULT, DISPATCH_MAX_CLAIMS_LIMIT, DeviceEntry, DeviceName, DeviceRole,
    DispatchClaimRequest, DispatchClaimResponse, DispatchHoldReason, DispatchHoldSummary,
    EventType, HousekeepingAction, JudgeKind, LEGACY_USAGE_ACCOUNT, LEGACY_USAGE_PROVIDER, Note,
    Priority, ResourceWeight, Run, RunId, RunKind, RunOrigin, RunState, SchedulerPolicy,
    SchedulerWeights, ScoreBreakdown, Taint, TaintEffect, TaintToleration, Task, TaskAffinity,
    TaskId, Timestamp, Usage,
};
use chrono::DateTime;
use rusqlite::{OptionalExtension, named_params, params};

use crate::db::{Ctx, enum_str, parse_enum, parse_id, parse_json, proposal_sql, ready_sql, ts};
use crate::error::{Result, invalid};
use crate::ops::devices::{check_token_list, get_active_entry};
use crate::ops::expires;
use crate::ops::judges::LIVE as LIVE_JUDGE;
use crate::ops::runs::{
    RUN_COLUMNS, acquire_conflict_locks, get as get_run, has_after_merge_checks,
    has_after_merge_ready_note, new_run_id, run_row, standing_review_pass,
};
use crate::ops::tasks::{ENTERED_DOING, LAST_RUN, delegate_at_work};
use crate::ops::usage::{get_bucket, validate_bucket_key};
use crate::store::{list_boards, load_tasks};
use crate::text::normalize_repo;

/// Maximum active judges allowed per board at once.
const JUDGES_AT_ONCE: usize = 2;
/// Maximum active verdict judges allowed across all off-autopilot boards together.
const HOME_VERDICTS_AT_ONCE: usize = 2;
/// Maximum proposals in one verdict batch or feedback items in one triage batch.
const JUDGE_TASKS_AT_ONCE: usize = 5;
/// Grace period after the server's repeat `idle_doing` notice before housekeeping releases.
const RELEASE_GRACE_MS: i64 = 5 * 60 * 1000;
/// Per-task merge cooldown after a deferred merge run whose checks failed (5 minutes).
const MERGE_COOLDOWN_CHECKS_FAILED_MS: i64 = 300 * 1000;
/// Per-task merge cooldown after a deferred or noop merge run (60 seconds).
const MERGE_COOLDOWN_DEFERRED_MS: i64 = 60 * 1000;
/// Per-task merge cooldown after a failed merge run (120 seconds).
const MERGE_COOLDOWN_FAILED_MS: i64 = 120 * 1000;
/// Rolling fair-share consumption window (24 hours).
const FAIR_SHARE_WINDOW_MS: i64 = 24 * 3_600_000;
/// Bounds matching `ops::devices`.
const DEVICE_UNITS_MAX: u32 = 10_000;
const DEVICE_RUNNERS_MAX: usize = 32;
const DEVICE_CAPABILITIES_MAX: usize = 64;
const DEVICE_REPOS_MAX: usize = 200;

/// All managed run kinds eligible for `POST /v1/dispatch/claim`.
const ALL_MANAGED_KINDS: [RunKind; 8] = [
    RunKind::Worker,
    RunKind::Review,
    RunKind::Verdict,
    RunKind::Triage,
    RunKind::Goal,
    RunKind::Merge,
    RunKind::Housekeeping,
    RunKind::Command,
];

/// Deduplication window for persisting `scheduler_holds` rows per `(board, task_id, reason)`.
const HOLD_DEDUP_MS: i64 = 3_600_000;

/// Validated `POST /v1/dispatch/claim` input passed into the `BEGIN IMMEDIATE` transaction.
#[derive(Debug, Clone)]
pub(crate) struct ValidatedClaim {
    pub device: DeviceName,
    pub free_units: u32,
    pub runners: Vec<String>,
    pub capabilities: Vec<String>,
    pub repos: Vec<String>,
    pub supported_kinds: Vec<RunKind>,
    pub quota_provider: String,
    pub quota_account: String,
    pub max_claims: u32,
}

/// Validates `POST /v1/dispatch/claim` at the API boundary.
pub(crate) fn validate_claim(req: DispatchClaimRequest) -> Result<ValidatedClaim> {
    if req.free_units > DEVICE_UNITS_MAX {
        return Err(invalid(
            format!("free_units={} is out of range", req.free_units),
            format!("send free_units between 0 and {DEVICE_UNITS_MAX}"),
        ));
    }
    let runners = check_token_list("runners", req.runners, DEVICE_RUNNERS_MAX, 40)?;
    let capabilities = check_token_list(
        "capabilities",
        req.capabilities,
        DEVICE_CAPABILITIES_MAX,
        64,
    )?;
    if req.repos.len() > DEVICE_REPOS_MAX {
        return Err(invalid(
            format!("a claim may advertise at most {DEVICE_REPOS_MAX} repos"),
            format!("send at most {DEVICE_REPOS_MAX} repos"),
        ));
    }
    let mut repos = Vec::with_capacity(req.repos.len());
    for raw in req.repos {
        let key = normalize_repo(&raw)?;
        if !repos.contains(&key) {
            repos.push(key);
        }
    }
    let mut supported_kinds = Vec::new();
    for kind in req.supported_kinds {
        if kind == RunKind::Unknown {
            return Err(invalid(
                "unknown run kind in supported_kinds",
                "send supported_kinds from worker, review, verdict, triage, goal, merge, housekeeping, command",
            ));
        }
        if kind == RunKind::Interactive {
            return Err(invalid(
                "interactive runs cannot be claimed via dispatch",
                "omit interactive from supported_kinds",
            ));
        }
        if !supported_kinds.contains(&kind) {
            supported_kinds.push(kind);
        }
    }
    if supported_kinds.is_empty() {
        supported_kinds.extend(ALL_MANAGED_KINDS);
    }
    let (quota_provider, quota_account) = match req.quota_bucket {
        Some(bucket) => (
            validate_bucket_key("quota_bucket.provider", &bucket.provider)?,
            validate_bucket_key("quota_bucket.account", &bucket.account)?,
        ),
        None => (
            LEGACY_USAGE_PROVIDER.to_owned(),
            LEGACY_USAGE_ACCOUNT.to_owned(),
        ),
    };
    let max_claims = req
        .max_claims
        .unwrap_or(DISPATCH_MAX_CLAIMS_DEFAULT)
        .clamp(1, DISPATCH_MAX_CLAIMS_LIMIT);

    Ok(ValidatedClaim {
        device: req.device,
        free_units: req.free_units,
        runners,
        capabilities,
        repos,
        supported_kinds,
        quota_provider,
        quota_account,
        max_claims,
    })
}

#[derive(Debug, Clone)]
struct Candidate {
    /// Stable deduplication key within this claim transaction.
    key: String,
    kind: RunKind,
    board: BoardSlug,
    task_id: Option<TaskId>,
    tasks: Vec<TaskId>,
    title: String,
    priority: Priority,
    rank: Option<u32>,
    created_at: Timestamp,
    cost_units: u32,
    weight_tier: ResourceWeight,
    repo: Option<String>,
    needs_local_repo: bool,
    zero_llm: bool,
    requires: Vec<String>,
    tolerations: Vec<TaintToleration>,
    affinity: Option<TaskAffinity>,
    preferred_runner: Option<String>,
    conflict_keys: Vec<ConflictKeySpec>,
    housekeeping_action: Option<HousekeepingAction>,
    existing_queued_run: Option<RunId>,
}

struct LiveJudgeRow {
    board: BoardSlug,
    agent: AgentId,
    kind: JudgeKind,
    tasks: Vec<TaskId>,
    running: bool,
}

/// Mutable in-transaction scheduler state updated after each grant in a multi-claim call.
struct SchedulerState {
    boards: HashMap<BoardSlug, Board>,
    /// Tasks currently in `doing` per board.
    doing_tasks_by_board: HashMap<BoardSlug, HashSet<TaskId>>,
    /// Tasks currently in `review` per board (for worker review-backlog backpressure).
    review_count_by_board: HashMap<BoardSlug, usize>,
    /// Active worker runs (`leased | starting | running`) whose `task_id` is not in `doing`
    /// (so `doing.len() + unclaimed_worker_runs` is the exact union count).
    unclaimed_worker_runs_by_board: HashMap<BoardSlug, usize>,
    /// Active judge count (`review | verdict | triage | goal`) per board.
    active_judges_by_board: HashMap<BoardSlug, usize>,
    /// Active off-autopilot verdict judges across all off-autopilot active boards.
    off_autopilot_verdicts: usize,
    /// Current `conflict_locks`: `key -> (holder_count, min_max_holders)`.
    conflict_locks: HashMap<String, (u32, u32)>,
    /// Recent 24h managed `cost_units` consumed per board (for fair-share).
    recent_board_units: HashMap<BoardSlug, u64>,
    /// Total recent 24h managed `cost_units` across all boards.
    total_recent_units: u64,
    /// Target LLM usage reading for `(quota_provider, quota_account)`.
    usage: Option<Usage>,
}

/// Runs the 5-stage scheduler pipeline inside one `BEGIN IMMEDIATE` SQLite transaction.
pub(crate) fn claim(
    cx: &mut Ctx<'_>,
    actor: &str,
    req: ValidatedClaim,
) -> Result<DispatchClaimResponse> {
    let device = get_active_entry(cx, &req.device)?;
    if device.drain {
        return Ok(DispatchClaimResponse {
            claimed: Vec::new(),
            holds_summary: vec![DispatchHoldSummary {
                reason: DispatchHoldReason::Drain,
                count: 1,
                detail: Some(format!("device {} is draining", device.name)),
            }],
        });
    }

    let eff_runners: &[String] = if req.runners.is_empty() {
        &device.runners
    } else {
        &req.runners
    };
    let eff_caps: &[String] = if req.capabilities.is_empty() {
        &device.capabilities
    } else {
        &req.capabilities
    };
    let eff_repos: &[String] = if req.repos.is_empty() {
        &device.repos
    } else {
        &req.repos
    };
    let eff_taints = effective_device_taints(&device);

    // Load global state once inside this transaction.
    let active_boards: Vec<Board> = list_boards(cx)?
        .into_iter()
        .filter(|b| b.status == BoardStatus::Active)
        .collect();
    if active_boards.is_empty() {
        return Ok(DispatchClaimResponse::default());
    }

    let active_policy = crate::ops::policy::active(cx)?;
    let active_runs = load_active_managed_runs(cx)?;
    let live_judges = load_live_judges(cx)?;
    let conflict_locks = load_conflict_locks(cx)?;
    let mut usage =
        get_bucket(cx, &req.quota_provider, &req.quota_account)?.and_then(|b| b.as_usage());
    if usage.is_none()
        && (req.quota_provider != LEGACY_USAGE_PROVIDER
            || req.quota_account != LEGACY_USAGE_ACCOUNT)
    {
        usage =
            get_bucket(cx, LEGACY_USAGE_PROVIDER, LEGACY_USAGE_ACCOUNT)?.and_then(|b| b.as_usage());
    }
    let (recent_board_units, total_recent_units) = load_recent_consumption(cx)?;

    // Doing tasks across active boards (for parallel_tasks union and blocker independence).
    let doing_tasks = load_tasks(
        cx,
        "t.status = 'doing' AND EXISTS (SELECT 1 FROM boards b WHERE b.slug = t.board AND b.status = 'active')",
        &Vec::new(),
    )?;
    let mut doing_tasks_by_board: HashMap<BoardSlug, HashSet<TaskId>> = HashMap::new();
    let mut blocked_by_in_progress: HashSet<TaskId> = HashSet::new();
    for t in &doing_tasks {
        doing_tasks_by_board
            .entry(t.board.clone())
            .or_default()
            .insert(t.id.clone());
        for b in &t.blockers {
            blocked_by_in_progress.insert(b.clone());
        }
    }

    // Also include blockers of tasks that already have an active managed worker run.
    let active_worker_task_ids: Vec<TaskId> = active_runs
        .iter()
        .filter(|r| r.kind == RunKind::Worker)
        .filter_map(|r| r.task_id.clone())
        .collect();
    if !active_worker_task_ids.is_empty() {
        let json_ids = crate::store::json_ids(active_worker_task_ids.iter().map(TaskId::as_str));
        let mut stmt = cx.db.prepare_cached(
            "SELECT blocker FROM task_blockers WHERE task IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = stmt.query_map([&json_ids], |r| parse_id::<TaskId>(&r.get::<_, String>(0)?))?;
        for row in rows {
            blocked_by_in_progress.insert(row?);
        }
    }

    let mut boards_map: HashMap<BoardSlug, Board> = HashMap::with_capacity(active_boards.len());
    for b in &active_boards {
        boards_map.insert(b.slug.clone(), b.clone());
    }

    // Compute worker union slots and judge counts.
    let mut unclaimed_worker_runs_by_board: HashMap<BoardSlug, usize> = HashMap::new();
    let mut active_judges_by_board: HashMap<BoardSlug, usize> = HashMap::new();
    let mut active_judge_agents: HashSet<(BoardSlug, AgentId)> = HashSet::new();
    let mut off_autopilot_verdicts: usize = 0;

    for run in &active_runs {
        let Some(ref board) = run.board else {
            continue;
        };
        if !boards_map.contains_key(board) {
            continue;
        }
        if run.kind == RunKind::Worker
            && matches!(
                run.state,
                RunState::Leased | RunState::Starting | RunState::Running
            )
        {
            let already_doing = run.task_id.as_ref().is_some_and(|tid| {
                doing_tasks_by_board
                    .get(board)
                    .is_some_and(|set| set.contains(tid))
            });
            if !already_doing {
                *unclaimed_worker_runs_by_board
                    .entry(board.clone())
                    .or_insert(0) += 1;
            }
        }
        if is_judge_kind(run.kind) && run.state.is_live() {
            *active_judges_by_board.entry(board.clone()).or_insert(0) += 1;
            if let Some(ref agent) = run.agent_id {
                active_judge_agents.insert((board.clone(), agent.clone()));
            }
            if run.kind == RunKind::Verdict && boards_map.get(board).is_some_and(|b| !b.autopilot) {
                off_autopilot_verdicts += 1;
            }
        }
    }
    for judge in &live_judges {
        if !judge.running || !boards_map.contains_key(&judge.board) {
            continue;
        }
        if active_judge_agents.contains(&(judge.board.clone(), judge.agent.clone())) {
            continue;
        }
        *active_judges_by_board
            .entry(judge.board.clone())
            .or_insert(0) += 1;
        if judge.kind == JudgeKind::Verdict
            && boards_map.get(&judge.board).is_some_and(|b| !b.autopilot)
        {
            off_autopilot_verdicts += 1;
        }
    }

    // Review tasks across active autopilot boards (for Review/Merge candidates and
    // worker review-backlog backpressure).
    let review_tasks = load_tasks(
        cx,
        "t.status = 'review' AND EXISTS (SELECT 1 FROM boards b WHERE b.slug = t.board AND b.status = 'active' AND b.autopilot = 1)",
        &Vec::new(),
    )?;
    let mut review_count_by_board: HashMap<BoardSlug, usize> = HashMap::new();
    for t in &review_tasks {
        *review_count_by_board.entry(t.board.clone()).or_insert(0) += 1;
    }

    // Stage 1: Candidate Enumeration across all active boards.
    let candidates = enumerate_candidates(
        cx,
        &active_boards,
        &boards_map,
        &active_runs,
        &live_judges,
        &conflict_locks,
        &doing_tasks,
        &review_tasks,
        &blocked_by_in_progress,
    )?;

    let mut state = SchedulerState {
        boards: boards_map,
        doing_tasks_by_board,
        review_count_by_board,
        unclaimed_worker_runs_by_board,
        active_judges_by_board,
        off_autopilot_verdicts,
        conflict_locks,
        recent_board_units,
        total_recent_units,
        usage,
    };

    let mut remaining_units = req.free_units;
    let mut claimed_keys: HashSet<String> = HashSet::new();
    let mut claimed_task_kinds: HashSet<(TaskId, RunKind)> = HashSet::new();
    let mut claimed: Vec<ClaimedRun> = Vec::new();
    let mut first_pass_holds: Vec<HoldRecord> = Vec::new();

    for claim_round in 0..req.max_claims {
        let mut round_holds: Vec<HoldRecord> = Vec::new();
        let mut eligible: Vec<(&Candidate, String, ScoreBreakdown)> = Vec::new();

        for cand in &candidates {
            if claimed_keys.contains(&cand.key) {
                continue;
            }
            if cand.kind != RunKind::Command
                && let Some(ref tid) = cand.task_id
                && claimed_task_kinds.contains(&(tid.clone(), cand.kind))
            {
                continue;
            }
            if !req.supported_kinds.contains(&cand.kind) {
                continue;
            }
            let Some(board) = state.boards.get(&cand.board) else {
                continue;
            };

            // Stage 2: Admission Gate (Kueue-style global constraints).
            if let Err((reason, detail, conflict_key)) = admit_candidate(cx, cand, board, &state) {
                round_holds.push(HoldRecord {
                    key: cand.key.clone(),
                    board: cand.board.clone(),
                    task_id: cand.task_id.clone(),
                    reason,
                    detail,
                    conflict_key,
                });
                continue;
            }

            // Stage 3: Filter (K8s predicates: taints, requires, runner, repo).
            let chosen_runner =
                match filter_candidate(cand, &eff_taints, eff_caps, eff_runners, eff_repos) {
                    Ok(r) => r,
                    Err((reason, detail)) => {
                        round_holds.push(HoldRecord {
                            key: cand.key.clone(),
                            board: cand.board.clone(),
                            task_id: cand.task_id.clone(),
                            reason,
                            detail,
                            conflict_key: None,
                        });
                        continue;
                    }
                };

            // Base multifactor score (before backfill) computed for both fitting and
            // capacity-blocked candidates so backfill knows if a higher-scoring heavier
            // candidate couldn't fit.
            let breakdown = compute_base_score(
                cx,
                cand,
                board,
                &device,
                &eff_taints,
                eff_repos,
                &state,
                &active_policy.weights,
            );
            eligible.push((cand, chosen_runner, breakdown));
        }

        // Capacity partition + Slurm backfill bonus.
        let mut max_blocked_score = f64::NEG_INFINITY;
        let mut fitting: Vec<(&Candidate, String, ScoreBreakdown)> = Vec::new();
        for (cand, runner, breakdown) in eligible {
            if cand.cost_units > remaining_units {
                let total = breakdown.total();
                if total > max_blocked_score {
                    max_blocked_score = total;
                }
                round_holds.push(HoldRecord {
                    key: cand.key.clone(),
                    board: cand.board.clone(),
                    task_id: cand.task_id.clone(),
                    reason: DispatchHoldReason::Capacity,
                    detail: format!(
                        "{} requires {} units, only {remaining_units} free",
                        cand.title, cand.cost_units
                    ),
                    conflict_key: None,
                });
            } else {
                fitting.push((cand, runner, breakdown));
            }
        }

        if claim_round == 0 {
            first_pass_holds = round_holds;
        }

        if fitting.is_empty() {
            break;
        }

        // Apply backfill bonus when a higher-scoring heavier candidate couldn't fit.
        for (cand, _, breakdown) in &mut fitting {
            if max_blocked_score.is_finite() && max_blocked_score > breakdown.total() {
                breakdown.backfill =
                    active_policy.weights.w_backfill / f64::from(cand.cost_units.max(1));
            }
        }

        // Stage 4: Rank fitting candidates deterministically (highest score first).
        fitting.sort_by(|(a, _, sa), (b, _, sb)| {
            sb.total()
                .total_cmp(&sa.total())
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.board.cmp(&b.board))
                .then_with(|| a.task_id.cmp(&b.task_id))
                .then_with(|| a.key.cmp(&b.key))
        });

        let (winner, chosen_runner, breakdown) = fitting.remove(0);

        // Stage 5: Atomic Grant.
        let granted = grant_candidate(
            cx,
            actor,
            &req.device,
            winner,
            &chosen_runner,
            breakdown,
            &active_policy,
        )?;
        remaining_units = remaining_units.saturating_sub(winner.cost_units);
        claimed_keys.insert(winner.key.clone());
        if winner.kind != RunKind::Command {
            if let Some(ref tid) = winner.task_id {
                claimed_task_kinds.insert((tid.clone(), winner.kind));
            }
            for tid in &winner.tasks {
                claimed_task_kinds.insert((tid.clone(), winner.kind));
            }
        }

        // Update in-transaction accounting for subsequent rounds in this same call.
        for spec in &winner.conflict_keys {
            let entry = state
                .conflict_locks
                .entry(spec.key.clone())
                .or_insert((0, spec.max));
            entry.0 += 1;
            entry.1 = entry.1.min(spec.max);
        }
        if winner.kind == RunKind::Worker {
            *state
                .unclaimed_worker_runs_by_board
                .entry(winner.board.clone())
                .or_insert(0) += 1;
        }
        if is_judge_kind(winner.kind) {
            *state
                .active_judges_by_board
                .entry(winner.board.clone())
                .or_insert(0) += 1;
            if winner.kind == RunKind::Verdict
                && state
                    .boards
                    .get(&winner.board)
                    .is_some_and(|b| !b.autopilot)
            {
                state.off_autopilot_verdicts += 1;
            }
        }
        *state
            .recent_board_units
            .entry(winner.board.clone())
            .or_insert(0) += u64::from(winner.cost_units);
        state.total_recent_units += u64::from(winner.cost_units);

        claimed.push(granted);
    }

    // Persist deduplicated rolling telemetry holds for unclaimed candidates, then summarize.
    record_scheduler_holds(
        cx,
        &req.device,
        &active_policy.version,
        &first_pass_holds,
        &claimed_keys,
    )?;
    let holds_summary = summarize_holds(first_pass_holds, &claimed_keys);
    Ok(DispatchClaimResponse {
        claimed,
        holds_summary,
    })
}

struct HoldRecord {
    key: String,
    board: BoardSlug,
    task_id: Option<TaskId>,
    reason: DispatchHoldReason,
    detail: String,
    conflict_key: Option<String>,
}

fn is_recorded_hold_reason(reason: DispatchHoldReason) -> bool {
    matches!(
        reason,
        DispatchHoldReason::Quota
            | DispatchHoldReason::ConflictKey
            | DispatchHoldReason::Capacity
            | DispatchHoldReason::Taint
            | DispatchHoldReason::Requires
    )
}

/// Persists compact hold telemetry rows (rate-limited to one row per `(board, task_id,
/// reason, conflict_key, policy_version)` per hour) so `GET /v1/stats/scheduler` and
/// `conflict_hot` pathology detection have exact empirical hold counts without bloating
/// the database.
fn record_scheduler_holds(
    cx: &Ctx<'_>,
    device: &DeviceName,
    policy_version: &str,
    holds: &[HoldRecord],
    claimed_keys: &HashSet<String>,
) -> Result<()> {
    let now = cx.now_ms();
    let cutoff = now.saturating_sub(HOLD_DEDUP_MS);
    let mut seen_this_call: HashSet<(&str, Option<&str>, DispatchHoldReason, Option<&str>)> =
        HashSet::new();
    for hold in holds {
        if claimed_keys.contains(&hold.key) || !is_recorded_hold_reason(hold.reason) {
            continue;
        }
        let task_str = hold.task_id.as_ref().map(TaskId::as_str);
        let ck_str = hold.conflict_key.as_deref();
        if !seen_this_call.insert((hold.board.as_str(), task_str, hold.reason, ck_str)) {
            continue;
        }
        let reason_str = enum_str(&hold.reason);
        let already: bool = cx
            .db
            .prepare_cached(
                "SELECT EXISTS ( \
                     SELECT 1 FROM scheduler_holds \
                     WHERE board IS ?1 AND task_id IS ?2 AND reason = ?3 \
                       AND conflict_key IS ?4 AND policy_version IS ?5 AND at > ?6 \
                 )",
            )?
            .query_row(
                params![
                    hold.board.as_str(),
                    task_str,
                    &reason_str,
                    ck_str,
                    policy_version,
                    cutoff
                ],
                |r| r.get(0),
            )?;
        if already {
            continue;
        }
        cx.db.execute(
            "INSERT INTO scheduler_holds ( \
                 at, board, task_id, device, reason, detail, conflict_key, policy_version \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                now,
                hold.board.as_str(),
                task_str,
                device.as_str(),
                reason_str,
                hold.detail,
                ck_str,
                policy_version,
            ],
        )?;
    }
    Ok(())
}

fn summarize_holds(
    holds: Vec<HoldRecord>,
    claimed_keys: &HashSet<String>,
) -> Vec<DispatchHoldSummary> {
    let mut grouped: BTreeMap<DispatchHoldReason, (u32, Option<String>)> = BTreeMap::new();
    for hold in holds {
        if claimed_keys.contains(&hold.key) {
            continue;
        }
        let entry = grouped.entry(hold.reason).or_insert((0, None));
        entry.0 += 1;
        if entry.1.is_none() {
            entry.1 = Some(hold.detail);
        }
    }
    grouped
        .into_iter()
        .map(|(reason, (count, detail))| DispatchHoldSummary {
            reason,
            count,
            detail,
        })
        .collect()
}

fn is_judge_kind(kind: RunKind) -> bool {
    matches!(
        kind,
        RunKind::Review | RunKind::Verdict | RunKind::Triage | RunKind::Goal
    )
}

pub(crate) fn effective_device_taints(device: &DeviceEntry) -> Vec<Taint> {
    let mut taints = device.taints.clone();
    let has_role_no_schedule = taints
        .iter()
        .any(|t| t.key == "role" && t.effect == TaintEffect::NoSchedule);
    if !has_role_no_schedule {
        match device.role {
            DeviceRole::Control => taints.push(Taint {
                key: "role".to_owned(),
                value: Some("control".to_owned()),
                effect: TaintEffect::NoSchedule,
            }),
            DeviceRole::ServiceHost => taints.push(Taint {
                key: "role".to_owned(),
                value: Some("service-host".to_owned()),
                effect: TaintEffect::NoSchedule,
            }),
            DeviceRole::Runner | DeviceRole::Unknown => {}
        }
    }
    taints
}

fn load_active_managed_runs(cx: &Ctx<'_>) -> Result<Vec<Run>> {
    Ok(cx
        .db
        .prepare_cached(&format!(
            "SELECT {RUN_COLUMNS} FROM runs r \
             WHERE r.origin = 'managed' \
               AND r.state IN ('queued', 'leased', 'starting', 'running', 'waiting_external') \
             ORDER BY r.created_at, r.id"
        ))?
        .query_map([], run_row)?
        .collect::<rusqlite::Result<_>>()?)
}

fn load_live_judges(cx: &Ctx<'_>) -> Result<Vec<LiveJudgeRow>> {
    let sql = format!(
        "SELECT j.board, j.agent, j.kind, j.tasks, \
                COALESCE((SELECT a.state = 'running' FROM agents a WHERE a.id = j.agent), 1) \
         FROM judges j WHERE {LIVE_JUDGE}"
    );
    Ok(cx
        .db
        .prepare_cached(&sql)?
        .query_map(named_params! { ":now": cx.now_ms() }, |r| {
            Ok(LiveJudgeRow {
                board: parse_id(&r.get::<_, String>(0)?)?,
                agent: parse_id(&r.get::<_, String>(1)?)?,
                kind: parse_enum(&r.get::<_, String>(2)?)?,
                tasks: parse_json(&r.get::<_, String>(3)?)?,
                running: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn load_conflict_locks(cx: &Ctx<'_>) -> Result<HashMap<String, (u32, u32)>> {
    let mut map = HashMap::new();
    let mut stmt = cx.db.prepare_cached(
        "SELECT key, COUNT(*), MIN(max_holders) FROM conflict_locks GROUP BY key",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        map.insert(
            r.get::<_, String>(0)?,
            (r.get::<_, u32>(1)?, r.get::<_, u32>(2)?),
        );
    }
    Ok(map)
}

fn load_recent_consumption(cx: &Ctx<'_>) -> Result<(HashMap<BoardSlug, u64>, u64)> {
    let since = cx.now_ms().saturating_sub(FAIR_SHARE_WINDOW_MS);
    let mut per_board = HashMap::new();
    let mut total: u64 = 0;
    let mut stmt = cx.db.prepare_cached(
        "SELECT board, COALESCE(SUM(cost_units), 0) FROM runs \
         WHERE board IS NOT NULL AND origin = 'managed' AND created_at >= ?1 \
         GROUP BY board",
    )?;
    let mut rows = stmt.query([since])?;
    while let Some(r) = rows.next()? {
        let board: BoardSlug = parse_id(&r.get::<_, String>(0)?)?;
        let units: i64 = r.get(1)?;
        let u = u64::try_from(units.max(0)).unwrap_or(0);
        per_board.insert(board, u);
        total = total.saturating_add(u);
    }
    Ok((per_board, total))
}

pub(crate) fn merge_conflict_keys(
    derived: Option<ConflictKeySpec>,
    explicit: &[ConflictKeySpec],
) -> Vec<ConflictKeySpec> {
    let mut out: Vec<ConflictKeySpec> = Vec::new();
    for spec in derived.into_iter().chain(explicit.iter().cloned()) {
        if let Some(existing) = out.iter_mut().find(|s| s.key == spec.key) {
            existing.max = existing.max.min(spec.max);
        } else {
            out.push(spec);
        }
    }
    out
}

pub(crate) fn task_weight_and_units(task: &Task) -> (ResourceWeight, u32) {
    let units = task
        .resource_units
        .or_else(|| task.weight.and_then(ResourceWeight::default_units))
        .unwrap_or(2);
    let tier = match task.weight {
        Some(w) if w != ResourceWeight::Unknown => w,
        _ => {
            if units >= 4 {
                ResourceWeight::Heavy
            } else if units <= 1 {
                ResourceWeight::Light
            } else {
                ResourceWeight::Standard
            }
        }
    };
    (tier, units)
}

fn verdict_lock_key(board: &BoardSlug, parent: Option<&TaskId>) -> String {
    match parent {
        Some(p) => format!("verdict:{board}:{p}"),
        None => format!("verdict:{board}:root"),
    }
}

#[allow(clippy::too_many_arguments)]
fn enumerate_candidates(
    cx: &Ctx<'_>,
    active_boards: &[Board],
    boards_map: &HashMap<BoardSlug, Board>,
    active_runs: &[Run],
    live_judges: &[LiveJudgeRow],
    conflict_locks: &HashMap<String, (u32, u32)>,
    doing_tasks: &[Task],
    review_tasks: &[Task],
    blocked_by_in_progress: &HashSet<TaskId>,
) -> Result<Vec<Candidate>> {
    let mut out: Vec<Candidate> = Vec::new();

    // Index active runs for quick exclusion checks.
    let mut active_task_kinds: HashSet<(TaskId, RunKind)> = HashSet::new();
    let mut active_boards_with_runs: HashSet<BoardSlug> = HashSet::new();
    let mut active_verdict_tasks: HashSet<TaskId> = HashSet::new();
    let mut active_triage_tasks: HashSet<TaskId> = HashSet::new();
    for run in active_runs {
        if let Some(ref b) = run.board {
            active_boards_with_runs.insert(b.clone());
        }
        if let Some(ref tid) = run.task_id {
            active_task_kinds.insert((tid.clone(), run.kind));
            if run.kind == RunKind::Verdict {
                active_verdict_tasks.insert(tid.clone());
            }
            if run.kind == RunKind::Triage {
                active_triage_tasks.insert(tid.clone());
            }
        }
    }

    let mut legacy_judge_tasks: HashSet<(JudgeKind, TaskId)> = HashSet::new();
    let mut legacy_boards_with_judges: HashSet<BoardSlug> = HashSet::new();
    let mut legacy_verdict_boards: HashSet<BoardSlug> = HashSet::new();
    for j in live_judges {
        legacy_boards_with_judges.insert(j.board.clone());
        if j.kind == JudgeKind::Verdict {
            legacy_verdict_boards.insert(j.board.clone());
        }
        for tid in &j.tasks {
            legacy_judge_tasks.insert((j.kind, tid.clone()));
        }
    }

    // Track per-board activity for goal review eligibility.
    let mut boards_with_doing: HashSet<BoardSlug> = HashSet::new();
    for t in doing_tasks {
        boards_with_doing.insert(t.board.clone());
    }
    let mut boards_with_review: HashSet<BoardSlug> = HashSet::new();
    let mut boards_with_ready_auto: HashSet<BoardSlug> = HashSet::new();
    let mut boards_with_proposals: HashSet<BoardSlug> = HashSet::new();
    let mut feedback_board_has_ready = false;

    // 1. Workers: ready `auto` non-proposal, non-feedback tasks on active autopilot boards.
    let ready_filter = format!(
        "{} AND t.auto = 1 \
         AND NOT EXISTS (SELECT 1 FROM feedback rf WHERE rf.task = t.id) \
         AND EXISTS (SELECT 1 FROM boards b WHERE b.slug = t.board AND b.status = 'active' AND b.autopilot = 1)",
        ready_sql("t")
    );
    let ready_auto = load_tasks(cx, &ready_filter, &Vec::new())?;
    for task in ready_auto {
        boards_with_ready_auto.insert(task.board.clone());
        if active_task_kinds.contains(&(task.id.clone(), RunKind::Worker)) {
            continue;
        }
        if blocked_by_in_progress.contains(&task.id) {
            continue;
        }
        let Some(board) = boards_map.get(&task.board) else {
            continue;
        };
        let repo = task.repo.clone().or_else(|| board.repos.first().cloned());
        let (weight_tier, cost_units) = task_weight_and_units(&task);
        let derived = repo.as_ref().map(|r| ConflictKeySpec {
            key: format!("repo:{r}:write"),
            max: board.parallel_tasks,
        });
        let conflict_keys = merge_conflict_keys(derived, &task.conflict_keys);
        out.push(Candidate {
            key: format!("worker:{}", task.id),
            kind: RunKind::Worker,
            board: task.board.clone(),
            task_id: Some(task.id.clone()),
            tasks: vec![task.id.clone()],
            title: format!("work {} {}", task.id, task.title),
            priority: task.priority,
            rank: task.rank,
            created_at: task.created_at,
            cost_units,
            weight_tier,
            repo,
            needs_local_repo: true,
            zero_llm: false,
            requires: task.requires,
            tolerations: task.tolerations,
            affinity: task.affinity,
            preferred_runner: task.preferred_runner,
            conflict_keys,
            housekeeping_action: None,
            existing_queued_run: None,
        });
    }

    // 2 & 3. Review & Merge: tasks in `review` on active autopilot boards.
    let latest_merge_runs = load_latest_terminal_merge_runs(cx, review_tasks)?;
    for task in review_tasks {
        boards_with_review.insert(task.board.clone());
        let Some(board) = boards_map.get(&task.board) else {
            continue;
        };
        let repo = task.repo.clone().or_else(|| board.repos.first().cloned());
        if let Some(pass_note) = standing_review_pass(cx, &task.id)? {
            let awaiting_after_merge = has_after_merge_checks(&pass_note.text)
                && has_after_merge_ready_note(cx, &task.id, pass_note.id)?;
            if awaiting_after_merge {
                // After-merge review candidate (LLM, standard cost 2): PR is already merged
                // (or `pr: none`), and the pre-merge pass listed `after merge:` checks.
                if active_task_kinds.contains(&(task.id.clone(), RunKind::Review))
                    || legacy_judge_tasks.contains(&(JudgeKind::Review, task.id.clone()))
                {
                    continue;
                }
                out.push(Candidate {
                    key: format!("review:{}", task.id),
                    kind: RunKind::Review,
                    board: task.board.clone(),
                    task_id: Some(task.id.clone()),
                    tasks: vec![task.id.clone()],
                    title: format!("after-merge {} {}", task.id, task.title),
                    priority: task.priority,
                    rank: task.rank,
                    created_at: task.updated_at,
                    cost_units: 2,
                    weight_tier: ResourceWeight::Standard,
                    repo,
                    needs_local_repo: true,
                    zero_llm: false,
                    requires: task.requires.clone(),
                    tolerations: task.tolerations.clone(),
                    affinity: task.affinity.clone(),
                    preferred_runner: task.preferred_runner.clone(),
                    conflict_keys: task.conflict_keys.clone(),
                    housekeeping_action: None,
                    existing_queued_run: None,
                });
            } else {
                // Merge candidate (zero-LLM, deterministic, cost 1, repo:<repo>:merge max 1).
                if active_task_kinds.contains(&(task.id.clone(), RunKind::Merge)) {
                    continue;
                }
                if let Some(latest_merge) = latest_merge_runs.get(&task.id)
                    && is_merge_in_cooldown(cx, &task.id, &pass_note, latest_merge)?
                {
                    continue;
                }
                let derived = repo.as_ref().map(|r| ConflictKeySpec {
                    key: format!("repo:{r}:merge"),
                    max: 1,
                });
                let conflict_keys = merge_conflict_keys(derived, &task.conflict_keys);
                out.push(Candidate {
                    key: format!("merge:{}", task.id),
                    kind: RunKind::Merge,
                    board: task.board.clone(),
                    task_id: Some(task.id.clone()),
                    tasks: vec![task.id.clone()],
                    title: format!("merge {} {}", task.id, task.title),
                    priority: task.priority,
                    rank: task.rank,
                    created_at: task.updated_at,
                    cost_units: 1,
                    weight_tier: ResourceWeight::Light,
                    repo,
                    needs_local_repo: false,
                    zero_llm: true,
                    requires: Vec::new(),
                    tolerations: task.tolerations.clone(),
                    affinity: task.affinity.clone(),
                    preferred_runner: Some("deterministic".to_owned()),
                    conflict_keys,
                    housekeeping_action: None,
                    existing_queued_run: None,
                });
            }
        } else {
            // Review candidate (LLM, standard cost 2).
            if active_task_kinds.contains(&(task.id.clone(), RunKind::Review))
                || legacy_judge_tasks.contains(&(JudgeKind::Review, task.id.clone()))
            {
                continue;
            }
            out.push(Candidate {
                key: format!("review:{}", task.id),
                kind: RunKind::Review,
                board: task.board.clone(),
                task_id: Some(task.id.clone()),
                tasks: vec![task.id.clone()],
                title: format!("review {} {}", task.id, task.title),
                priority: task.priority,
                rank: task.rank,
                created_at: task.updated_at,
                cost_units: 2,
                weight_tier: ResourceWeight::Standard,
                repo,
                needs_local_repo: true,
                zero_llm: false,
                requires: task.requires.clone(),
                tolerations: task.tolerations.clone(),
                affinity: task.affinity.clone(),
                preferred_runner: task.preferred_runner.clone(),
                conflict_keys: task.conflict_keys.clone(),
                housekeeping_action: None,
                existing_queued_run: None,
            });
        }
    }

    // 4. Housekeeping: (a) answered asks whose asker is gone; (b) doing tasks past repeat
    //    `idle_doing` + grace whose owner is away. Zero-LLM, deterministic, cost 1.
    enumerate_housekeeping(
        cx,
        &active_task_kinds,
        conflict_locks,
        doing_tasks,
        &mut out,
    )?;

    let debounce_ms = cx.s.judge_batch_debounce_ms();
    let min_batch =
        cx.s.effective_judge_batch_min_size()
            .min(JUDGE_TASKS_AT_ONCE);

    // 5. Verdict: pending proposals on active boards with `verdict_judges == true`
    //    (BOTH autopilot AND off-autopilot), grouped by `(board, parent)` (up to 5 oldest).
    let proposal_filter = format!(
        "{} AND EXISTS (SELECT 1 FROM boards b WHERE b.slug = t.board AND b.status = 'active' AND b.verdict_judges = 1)",
        proposal_sql("t")
    );
    let proposals = load_tasks(cx, &proposal_filter, &Vec::new())?;
    let mut by_board_parent: BTreeMap<(BoardSlug, Option<TaskId>), Vec<Task>> = BTreeMap::new();
    for p in proposals {
        boards_with_proposals.insert(p.board.clone());
        by_board_parent
            .entry((p.board.clone(), p.parent.clone()))
            .or_default()
            .push(p);
    }
    for ((board_slug, parent), mut group) in by_board_parent {
        let Some(board) = boards_map.get(&board_slug) else {
            continue;
        };
        let lock_key = verdict_lock_key(&board_slug, parent.as_ref());
        if conflict_locks.contains_key(&lock_key) {
            continue;
        }
        if !board.autopilot && legacy_verdict_boards.contains(&board_slug) {
            continue;
        }
        if group.iter().any(|t| {
            active_verdict_tasks.contains(&t.id)
                || legacy_judge_tasks.contains(&(JudgeKind::Verdict, t.id.clone()))
        }) {
            continue;
        }
        group.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let batch: Vec<Task> = group.into_iter().take(JUDGE_TASKS_AT_ONCE).collect();
        let Some(first) = batch.first() else {
            continue;
        };
        let priority = batch
            .iter()
            .map(|t| t.priority)
            .min()
            .unwrap_or(Priority::P2);
        if debounce_ms > 0
            && batch.len() < min_batch
            && priority != Priority::P1
            && cx
                .now_ms()
                .saturating_sub(first.created_at.timestamp_millis())
                < debounce_ms
        {
            continue;
        }
        let repo = first.repo.clone().or_else(|| board.repos.first().cloned());
        let under = parent
            .as_ref()
            .map_or_else(|| "no parent".to_owned(), |p| format!("under {p}"));
        out.push(Candidate {
            key: lock_key.clone(),
            kind: RunKind::Verdict,
            board: board_slug.clone(),
            task_id: Some(first.id.clone()),
            tasks: batch.iter().map(|t| t.id.clone()).collect(),
            title: format!("verdict {} ({under})", first.id),
            priority,
            rank: first.rank,
            created_at: first.created_at,
            cost_units: 1,
            weight_tier: ResourceWeight::Light,
            repo,
            needs_local_repo: true,
            zero_llm: false,
            requires: Vec::new(),
            tolerations: first.tolerations.clone(),
            affinity: first.affinity.clone(),
            preferred_runner: first.preferred_runner.clone(),
            conflict_keys: vec![ConflictKeySpec {
                key: lock_key,
                max: 1,
            }],
            housekeeping_action: None,
            existing_queued_run: None,
        });
    }

    // 6. Triage: ready feedback items on `feedback_board` when active & autopilot on.
    if let Some(fb_board) = boards_map.get(&cx.s.feedback_board)
        && fb_board.autopilot
    {
        let fb_slug = fb_board.slug.as_str();
        let fb_filter = format!(
            "t.board = :board AND {} AND EXISTS (SELECT 1 FROM feedback ff WHERE ff.task = t.id)",
            ready_sql("t")
        );
        let mut fb_items = load_tasks(cx, &fb_filter, &vec![(":board", &fb_slug)])?;
        if !fb_items.is_empty() {
            feedback_board_has_ready = true;
        }
        fb_items.retain(|t| {
            !active_triage_tasks.contains(&t.id)
                && !conflict_locks.contains_key(&format!("triage:{}", t.id))
                && !legacy_judge_tasks.contains(&(JudgeKind::Triage, t.id.clone()))
        });
        fb_items.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for chunk in fb_items.chunks(JUDGE_TASKS_AT_ONCE) {
            let first = &chunk[0];
            let priority = chunk
                .iter()
                .map(|t| t.priority)
                .min()
                .unwrap_or(Priority::P2);
            if debounce_ms > 0
                && chunk.len() < min_batch
                && priority != Priority::P1
                && cx
                    .now_ms()
                    .saturating_sub(first.created_at.timestamp_millis())
                    < debounce_ms
            {
                continue;
            }
            let repo = first
                .repo
                .clone()
                .or_else(|| fb_board.repos.first().cloned());
            let conflict_keys = chunk
                .iter()
                .map(|t| ConflictKeySpec {
                    key: format!("triage:{}", t.id),
                    max: 1,
                })
                .collect();
            out.push(Candidate {
                key: format!("triage:{}", first.id),
                kind: RunKind::Triage,
                board: fb_board.slug.clone(),
                task_id: Some(first.id.clone()),
                tasks: chunk.iter().map(|t| t.id.clone()).collect(),
                title: format!("triage {}", first.id),
                priority,
                rank: first.rank,
                created_at: first.created_at,
                cost_units: 1,
                weight_tier: ResourceWeight::Light,
                repo,
                needs_local_repo: true,
                zero_llm: false,
                requires: Vec::new(),
                tolerations: first.tolerations.clone(),
                affinity: first.affinity.clone(),
                preferred_runner: first.preferred_runner.clone(),
                conflict_keys,
                housekeeping_action: None,
                existing_queued_run: None,
            });
        }
    }

    // 7. Goal: active autopilot boards with non-empty goal, at least one `task_done` since
    //    last goal review started, past any per-board `goal_review_every_secs` cooldown,
    //    and nothing in progress/review/ready-auto/waiting-for-judge.
    for board in active_boards {
        if !board.autopilot || board.goal.trim().is_empty() {
            continue;
        }
        let slug = &board.slug;
        let has_pending_triage = *slug == cx.s.feedback_board && feedback_board_has_ready;
        if boards_with_doing.contains(slug)
            || boards_with_review.contains(slug)
            || boards_with_ready_auto.contains(slug)
            || (board.verdict_judges && boards_with_proposals.contains(slug))
            || has_pending_triage
            || active_boards_with_runs.contains(slug)
            || legacy_boards_with_judges.contains(slug)
        {
            continue;
        }
        let Some(last_done_at) = latest_board_task_done_ms(cx, slug)? else {
            continue;
        };
        let (last_goal_started_at, last_goal_at) = latest_board_goal_review_times(cx, slug)?;
        if last_goal_started_at.is_some_and(|g| g >= last_done_at) {
            continue;
        }
        if let Some(interval_secs) =
            crate::ops::boards::effective_goal_review_every_secs(board, &cx.s.feedback_board)
            && let Some(last_at) = last_goal_at
        {
            let interval_ms = i64::try_from(interval_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
            if cx.now_ms().saturating_sub(last_at) < interval_ms {
                continue;
            }
        }
        let lock_key = format!("goal:{slug}");
        if conflict_locks.contains_key(&lock_key) {
            continue;
        }
        out.push(Candidate {
            key: lock_key.clone(),
            kind: RunKind::Goal,
            board: slug.clone(),
            task_id: None,
            tasks: Vec::new(),
            title: format!("goal review {slug}"),
            priority: Priority::P3,
            rank: None,
            created_at: ts(last_done_at),
            cost_units: 1,
            weight_tier: ResourceWeight::Light,
            repo: board.repos.first().cloned(),
            needs_local_repo: true,
            zero_llm: false,
            requires: Vec::new(),
            tolerations: Vec::new(),
            affinity: None,
            preferred_runner: None,
            conflict_keys: vec![ConflictKeySpec {
                key: lock_key,
                max: 1,
            }],
            housekeeping_action: None,
            existing_queued_run: None,
        });
    }

    // 8. Explicitly `queued` `command` runs on active autopilot boards.
    for run in active_runs {
        if run.kind != RunKind::Command || run.state != RunState::Queued {
            continue;
        }
        let Some(ref board_slug) = run.board else {
            continue;
        };
        let Some(board) = boards_map.get(board_slug) else {
            continue;
        };
        if !board.autopilot {
            continue;
        }
        let linked_task = match &run.task_id {
            Some(tid) => crate::store::find_task(cx, tid)?,
            None => None,
        };
        let repo = linked_task
            .as_ref()
            .and_then(|t| t.repo.clone())
            .or_else(|| board.repos.first().cloned());
        let zero_llm = run.runner == "deterministic";
        let weight_tier = if run.cost_units >= 4 {
            ResourceWeight::Heavy
        } else if run.cost_units <= 1 {
            ResourceWeight::Light
        } else {
            ResourceWeight::Standard
        };
        let (priority, rank, requires, tolerations, affinity, conflict_keys) = match linked_task {
            Some(t) => (
                t.priority,
                t.rank,
                t.requires,
                t.tolerations,
                t.affinity,
                t.conflict_keys,
            ),
            None => (Priority::P2, None, Vec::new(), Vec::new(), None, Vec::new()),
        };
        out.push(Candidate {
            key: format!("command:{}", run.id),
            kind: RunKind::Command,
            board: board_slug.clone(),
            task_id: run.task_id.clone(),
            tasks: run.task_id.iter().cloned().collect(),
            title: run
                .title
                .clone()
                .unwrap_or_else(|| format!("command {}", run.id)),
            priority,
            rank,
            created_at: run.created_at,
            cost_units: run.cost_units.max(1),
            weight_tier,
            repo,
            needs_local_repo: !zero_llm,
            zero_llm,
            requires,
            tolerations,
            affinity,
            preferred_runner: Some(run.runner.clone()),
            conflict_keys,
            housekeeping_action: None,
            existing_queued_run: Some(run.id.clone()),
        });
    }

    Ok(out)
}

fn enumerate_housekeeping(
    cx: &Ctx<'_>,
    active_task_kinds: &HashSet<(TaskId, RunKind)>,
    conflict_locks: &HashMap<String, (u32, u32)>,
    doing_tasks: &[Task],
    out: &mut Vec<Candidate>,
) -> Result<()> {
    let now = cx.now_ms();
    let cutoff = cx.stale_cutoff();

    // (a) Answered asks on active autopilot boards whose asking agent is gone/unreachable
    //     while the task is still held (`doing | blocked | review`) and no live managed run
    //     or pending sticky wake is actively handling wake-up for that agent.
    let gone_asks: Vec<(i64, BoardSlug, TaskId, AgentId, Priority, i64)> = cx
        .db
        .prepare_cached(
            "SELECT a.id, a.board, a.task, a.agent, t.priority, COALESCE(a.answered_at, a.created_at) \
             FROM asks a \
             JOIN boards b ON b.slug = a.board \
             JOIN tasks t ON t.id = a.task \
             LEFT JOIN agents ag ON ag.id = a.agent \
             WHERE a.status = 'answered' \
               AND a.task IS NOT NULL \
               AND b.status = 'active' AND b.autopilot = 1 \
               AND ( \
                   ( \
                       t.status IN ('doing', 'blocked', 'review') \
                       AND (t.owner IS NULL OR t.owner = a.agent) \
                       AND ( \
                           ag.id IS NULL \
                           OR (ag.state = 'ended' AND (ag.paused_until IS NULL OR ag.paused_until <= ?1)) \
                           OR (ag.last_seen <= ?2 AND NOT (ag.state = 'paused' AND ag.paused_until > ?1)) \
                       ) \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM runs r \
                           WHERE r.agent_id = a.agent \
                             AND r.origin = 'managed' \
                             AND r.state IN ('leased', 'starting', 'running', 'waiting_external') \
                             AND (r.lease_expires_at IS NULL OR r.lease_expires_at > ?1) \
                       ) \
                   ) \
                   OR ( \
                       substr(a.agent, 1, 9) = 'dispatch:' \
                       AND t.status = 'blocked' \
                       AND t.blocked_reason IS NOT NULL \
                       AND instr(t.blocked_reason, 'ask #' || a.id || ':') > 0 \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM asks oa \
                           WHERE oa.task = t.id AND oa.status = 'open' \
                       ) \
                   ) \
               ) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM sticky_wakes sw \
                   WHERE sw.agent_id = a.agent \
                     AND sw.delivered_at IS NULL \
                     AND (sw.expires_at IS NULL OR sw.expires_at > ?1) \
               ) \
             ORDER BY a.answered_at, a.id",
        )?
        .query_map(params![now, cutoff], |r| {
            Ok((
                r.get(0)?,
                parse_id(&r.get::<_, String>(1)?)?,
                parse_id(&r.get::<_, String>(2)?)?,
                parse_id(&r.get::<_, String>(3)?)?,
                parse_enum(&r.get::<_, String>(4)?)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    for (ask_id, board, task_id, asker, priority, answered_at) in gone_asks {
        let lock_key = format!("housekeeping:ask:{ask_id}");
        if conflict_locks.contains_key(&lock_key)
            || active_task_kinds.contains(&(task_id.clone(), RunKind::Housekeeping))
        {
            continue;
        }
        // Skip non-dispatch asks if a decision note already recorded this ask's answer after
        // it was answered. Dispatch asks stay gated on `t.status = 'blocked'` matching
        // `ask #<id>:` so a transient failure after writing the note still retries the transition.
        if asker.source() != board_proto::AgentSource::Dispatch {
            let marker = format!("ask #{ask_id}");
            let already_noted: bool = cx
                .db
                .prepare_cached(
                    "SELECT EXISTS ( \
                         SELECT 1 FROM notes \
                         WHERE task = ?1 AND kind = 'decision' AND at >= ?2 \
                           AND instr(text, ?3) > 0 \
                     )",
                )?
                .query_row(params![task_id.as_str(), answered_at, marker], |r| r.get(0))?;
            if already_noted {
                continue;
            }
        }
        out.push(Candidate {
            key: lock_key.clone(),
            kind: RunKind::Housekeeping,
            board,
            task_id: Some(task_id.clone()),
            tasks: vec![task_id.clone()],
            title: format!("housekeeping answer ask #{ask_id} for {task_id}"),
            priority,
            rank: None,
            created_at: ts(answered_at),
            cost_units: 1,
            weight_tier: ResourceWeight::Light,
            repo: None,
            needs_local_repo: false,
            zero_llm: true,
            requires: Vec::new(),
            tolerations: Vec::new(),
            affinity: None,
            preferred_runner: Some("deterministic".to_owned()),
            conflict_keys: vec![ConflictKeySpec {
                key: lock_key,
                max: 1,
            }],
            housekeeping_action: Some(HousekeepingAction::AnswerGoneAsker {
                ask_id,
                task_id,
                asker: Some(asker),
            }),
            existing_queued_run: None,
        });
    }

    // (b) `doing` tasks on active autopilot boards past repeat `idle_doing` + grace whose
    //     owner is still away.
    for task in doing_tasks {
        let Some(ref owner) = task.owner else {
            continue;
        };
        let is_autopilot: bool = cx
            .db
            .prepare_cached("SELECT autopilot FROM boards WHERE slug = ?1 AND status = 'active'")?
            .query_row([task.board.as_str()], |r| r.get(0))
            .optional()?
            .unwrap_or(false);
        if !is_autopilot {
            continue;
        }
        let lock_key = format!("housekeeping:task:{}", task.id);
        if conflict_locks.contains_key(&lock_key)
            || active_task_kinds.contains(&(task.id.clone(), RunKind::Housekeeping))
        {
            continue;
        }
        let agent_info: Option<(String, i64, Option<i64>)> = cx
            .db
            .prepare_cached("SELECT state, last_seen, paused_until FROM agents WHERE id = ?1")?
            .query_row([owner.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((state, last_seen, paused_until)) = agent_info else {
            continue;
        };
        let waits_for_reset =
            matches!(state.as_str(), "paused" | "ended") && paused_until.is_some_and(|p| p > now);
        if waits_for_reset {
            continue;
        }
        let last_run: i64 = cx
            .db
            .prepare_cached(LAST_RUN)?
            .query_row([owner.as_str()], |r| r.get(0))?;
        let entered: i64 = cx
            .db
            .prepare_cached(ENTERED_DOING)?
            .query_row([task.id.as_str()], |r| r.get(0))?;
        let running = state == "running";
        // Match `ops::tasks::announce_idle_doing` spell boundaries: group by `idle_doing`
        // spell timestamp and, when `running`, only count notices at/after `last_seen`.
        let spell: Option<(i64, i64, String)> = cx
            .db
            .prepare_cached(
                "SELECT COUNT(*), MAX(at), json_extract(payload, '$.idle_doing') AS spell \
                 FROM events \
                 WHERE task = ?1 AND seq > ?2 AND type = 'task_updated' \
                   AND json_extract(payload, '$.idle_doing') IS NOT NULL \
                   AND json_extract(payload, '$.away') = ?3 \
                   AND (?4 IS NULL OR at >= ?4) \
                 GROUP BY spell \
                 ORDER BY MAX(seq) DESC \
                 LIMIT 1",
            )?
            .query_row(
                params![
                    task.id.as_str(),
                    last_run.max(entered),
                    owner.as_str(),
                    running.then_some(last_seen)
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((count, noticed_at, idle_since)) = spell else {
            continue;
        };
        let away_since = DateTime::parse_from_rfc3339(&idle_since)
            .ok()
            .map(|dt| dt.to_utc());
        // Release after either the repeat notice (`count >= 2`) or a late first notice
        // recorded at or past twice `doing_idle_secs`, for which `announce_idle_doing`
        // emits no separate repeat.
        let twice_ms = away_since.map(|dt| {
            dt.timestamp_millis()
                .saturating_add(2 * cx.s.doing_idle_ms())
        });
        let late_first = count >= 1 && twice_ms.is_some_and(|twice| noticed_at >= twice);
        if count < 2 && !late_first {
            continue;
        }
        if now < noticed_at.saturating_add(RELEASE_GRACE_MS) {
            continue;
        }
        // A transition back to `running` already advanced `last_run` and excluded the spell
        // above, and an `ended` agent's API call revives it to `running` via `touch_agent`.
        // Keep the `last_seen > noticed_at` guard for non-`ended` states so an `idle` or
        // `paused` owner calling board APIs during the grace window is not released, while
        // still letting T3's idle-to-`ended` session close proceed.
        if state != "ended" && last_seen > noticed_at {
            continue;
        }
        if delegate_at_work(cx, owner)? {
            continue;
        }
        out.push(Candidate {
            key: lock_key.clone(),
            kind: RunKind::Housekeeping,
            board: task.board.clone(),
            task_id: Some(task.id.clone()),
            tasks: vec![task.id.clone()],
            title: format!("housekeeping release away owner of {}", task.id),
            priority: task.priority,
            rank: None,
            created_at: ts(noticed_at),
            cost_units: 1,
            weight_tier: ResourceWeight::Light,
            repo: None,
            needs_local_repo: false,
            zero_llm: true,
            requires: Vec::new(),
            tolerations: Vec::new(),
            affinity: None,
            preferred_runner: Some("deterministic".to_owned()),
            conflict_keys: vec![ConflictKeySpec {
                key: lock_key,
                max: 1,
            }],
            housekeeping_action: Some(HousekeepingAction::ReleaseAwayOwner {
                task_id: task.id.clone(),
                owner: owner.clone(),
                away_since,
            }),
            existing_queued_run: None,
        });
    }

    Ok(())
}

fn latest_board_task_done_ms(cx: &Ctx<'_>, board: &BoardSlug) -> Result<Option<i64>> {
    Ok(cx
        .db
        .prepare_cached("SELECT MAX(at) FROM events WHERE board = ?1 AND type = 'task_done'")?
        .query_row([board.as_str()], |r| r.get(0))?)
}

/// Returns `(started_ms, completed_ms)` of the latest succeeded goal review on `board`,
/// combining managed `runs` (`COALESCE(started_at, created_at)` and
/// `COALESCE(ended_at, started_at, created_at)`) with legacy `board_updated` goal events in
/// two queries total.
fn latest_board_goal_review_times(
    cx: &Ctx<'_>,
    board: &BoardSlug,
) -> Result<(Option<i64>, Option<i64>)> {
    let (run_started, run_completed): (Option<i64>, Option<i64>) = cx
        .db
        .prepare_cached(
            "SELECT MAX(COALESCE(started_at, created_at)), \
                    MAX(COALESCE(ended_at, started_at, created_at)) \
             FROM runs \
             WHERE board = ?1 AND kind = 'goal' AND state = 'succeeded'",
        )?
        .query_row([board.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let event_at: Option<i64> = cx
        .db
        .prepare_cached(
            "SELECT MAX(at) FROM events \
             WHERE board = ?1 AND type = 'board_updated' \
               AND json_extract(payload, '$.kind') = 'goal'",
        )?
        .query_row([board.as_str()], |r| r.get(0))?;
    let merge = |a: Option<i64>, b: Option<i64>| match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    };
    Ok((merge(run_started, event_at), merge(run_completed, event_at)))
}

/// Worker review-backlog cap for a board (`max(3, parallel_tasks * 2)`).
pub(crate) fn review_backlog_limit(parallel_tasks: u32) -> usize {
    (parallel_tasks.max(1) as usize * 2).max(3)
}

struct LatestMergeRun {
    id: String,
    state: RunState,
    outcome: Option<serde_json::Value>,
    created_at_ms: i64,
    ended_at_ms: i64,
}

fn load_latest_terminal_merge_runs(
    cx: &Ctx<'_>,
    review_tasks: &[Task],
) -> Result<HashMap<TaskId, LatestMergeRun>> {
    if review_tasks.is_empty() {
        return Ok(HashMap::new());
    }
    let json_ids = crate::store::json_ids(review_tasks.iter().map(|t| t.id.as_str()));
    let mut stmt = cx.db.prepare_cached(
        "SELECT r.task_id, r.id, r.state, r.outcome_json, r.created_at, \
                COALESCE(r.ended_at, r.updated_at) AS ended_at \
         FROM runs r \
         WHERE r.task_id IN (SELECT value FROM json_each(?1)) \
           AND r.kind = 'merge' \
           AND r.state IN ('succeeded', 'failed', 'preempted', 'orphaned', 'cancelled') \
         ORDER BY r.task_id, r.created_at DESC, COALESCE(r.ended_at, r.updated_at) DESC, r.rowid DESC",
    )?;
    let mut out = HashMap::new();
    let mut rows = stmt.query([&json_ids])?;
    while let Some(r) = rows.next()? {
        let task_id: TaskId = parse_id(&r.get::<_, String>(0)?)?;
        if out.contains_key(&task_id) {
            continue;
        }
        let outcome = r
            .get::<_, Option<String>>(3)?
            .map(|s| parse_json(&s))
            .transpose()?;
        out.insert(
            task_id,
            LatestMergeRun {
                id: r.get(1)?,
                state: parse_enum(&r.get::<_, String>(2)?)?,
                outcome,
                created_at_ms: r.get(4)?,
                ended_at_ms: r.get(5)?,
            },
        );
    }
    Ok(out)
}

/// Returns `true` when `latest_merge` places `task_id` in per-task merge cooldown or
/// `unclear` suppression for the current standing `pass_note`.
fn is_merge_in_cooldown(
    cx: &Ctx<'_>,
    task_id: &TaskId,
    pass_note: &Note,
    latest_merge: &LatestMergeRun,
) -> Result<bool> {
    let pass_note_seq: Option<i64> = cx
        .db
        .prepare_cached(
            "SELECT seq FROM events \
             WHERE task = ?1 AND type = 'note_added' \
               AND json_extract(payload, '$.note') = ?2 \
             ORDER BY seq DESC LIMIT 1",
        )?
        .query_row(params![task_id.as_str(), pass_note.id], |r| r.get(0))
        .optional()?;
    let latest_review: Option<(i64, i64)> = cx
        .db
        .prepare_cached(
            "SELECT seq, at FROM events \
             WHERE task = ?1 AND type = 'task_updated' \
               AND json_extract(payload, '$.to') = 'review' \
             ORDER BY seq DESC LIMIT 1",
        )?
        .query_row([task_id.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (merge_created_seq, merge_ended_seq): (Option<i64>, Option<i64>) = cx
        .db
        .prepare_cached(
            "SELECT MIN(seq), MAX(seq) FROM events \
             WHERE task = ?1 AND type IN ('run_created', 'run_updated') \
               AND json_extract(payload, '$.run_id') = ?2",
        )?
        .query_row(params![task_id.as_str(), &latest_merge.id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;

    // Ignore merge runs that predate the current standing pass note or a newer move to review.
    let before_pass = match (merge_created_seq, pass_note_seq) {
        (Some(m_seq), Some(p_seq)) => m_seq < p_seq,
        _ => latest_merge.created_at_ms < pass_note.at.timestamp_millis(),
    };
    if before_pass {
        return Ok(false);
    }
    if let Some((review_seq, review_at_ms)) = latest_review {
        let before_review = match merge_created_seq {
            Some(m_seq) => m_seq < review_seq,
            None => latest_merge.created_at_ms < review_at_ms,
        };
        if before_review {
            return Ok(false);
        }
    }

    let outcome_status = latest_merge
        .outcome
        .as_ref()
        .and_then(|v| v.get("status"))
        .and_then(serde_json::Value::as_str);
    let outcome_reason = latest_merge
        .outcome
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(serde_json::Value::as_str);

    // 1. `unclear` merge outcome: suppress until a newer `review: pass` note or newer move
    //    to `review` (checked above).
    if latest_merge.state == RunState::Succeeded && outcome_status == Some("unclear") {
        return Ok(true);
    }

    // 2 & 3. Timed cooldown for `deferred`/`noop` (60s, or 300s when checks failed) and
    //    `failed` (120s), cleared early by a newer `note_added` or `task_updated` event on
    //    this task after the merge run ended.
    let cooldown_ms = match latest_merge.state {
        RunState::Succeeded if matches!(outcome_status, Some("deferred" | "noop")) => {
            if outcome_reason.is_some_and(|r| r.starts_with("checks failed")) {
                MERGE_COOLDOWN_CHECKS_FAILED_MS
            } else {
                MERGE_COOLDOWN_DEFERRED_MS
            }
        }
        RunState::Failed => MERGE_COOLDOWN_FAILED_MS,
        _ => return Ok(false),
    };

    if cx.now_ms() >= latest_merge.ended_at_ms.saturating_add(cooldown_ms) {
        return Ok(false);
    }

    let newer_activity: bool = cx
        .db
        .prepare_cached(
            "SELECT EXISTS ( \
                 SELECT 1 FROM events \
                 WHERE task = ?1 \
                   AND type IN ('note_added', 'task_updated') \
                   AND ( \
                       (?2 IS NOT NULL AND seq > ?2) \
                       OR (?2 IS NULL AND at > ?3) \
                   ) \
             )",
        )?
        .query_row(
            params![task_id.as_str(), merge_ended_seq, latest_merge.ended_at_ms],
            |r| r.get(0),
        )?;
    Ok(!newer_activity)
}

/// Stage 2 — Kueue-style global admission gate.
fn admit_candidate(
    cx: &Ctx<'_>,
    cand: &Candidate,
    board: &Board,
    state: &SchedulerState,
) -> std::result::Result<(), (DispatchHoldReason, String, Option<String>)> {
    // LLM quota & pace gate (zero-LLM deterministic runs are exempt).
    let mut pace_limit = board.parallel_tasks;
    if !cand.zero_llm
        && let Some(ref usage) = state.usage
    {
        if let Some((win_kind, win)) = usage.over(board.usage_stop_at, cx.now) {
            return Err((
                DispatchHoldReason::Quota,
                format!(
                    "board {} over usage_stop_at ({} {:.1}% >= {}%)",
                    board.slug,
                    win_kind.label(),
                    win.percent,
                    board.usage_stop_at
                ),
                None,
            ));
        }
        pace_limit = usage.pace_limit(board.parallel_tasks, cx.now);
        if pace_limit == 0 {
            return Err((
                DispatchHoldReason::Quota,
                format!("board {} held by weekly usage pace limit (0)", board.slug),
                None,
            ));
        }
    }

    // Worker-only `parallel_tasks` cap and review-backlog backpressure. Never blocks
    // review/merge/verdict/triage/goal/housekeeping!
    if cand.kind == RunKind::Worker {
        let doing_count = state
            .doing_tasks_by_board
            .get(&board.slug)
            .map_or(0, HashSet::len);
        let unclaimed = state
            .unclaimed_worker_runs_by_board
            .get(&board.slug)
            .copied()
            .unwrap_or(0);
        let in_flight = doing_count + unclaimed;
        let cap = board.parallel_tasks.min(pace_limit) as usize;
        if in_flight >= cap {
            return Err((
                DispatchHoldReason::ParallelTasks,
                format!(
                    "board {} at parallel_tasks limit ({in_flight}/{cap})",
                    board.slug
                ),
                None,
            ));
        }
        let review_count = state
            .review_count_by_board
            .get(&board.slug)
            .copied()
            .unwrap_or(0);
        let review_backlog_limit = review_backlog_limit(board.parallel_tasks);
        if review_count >= review_backlog_limit {
            return Err((
                DispatchHoldReason::Capacity,
                format!(
                    "board {} paused new workers while {review_count} tasks wait in review (limit {review_backlog_limit})",
                    board.slug
                ),
                None,
            ));
        }
    }

    // Judge concurrency caps (`JUDGES_AT_ONCE = 2` per board; `HOME_VERDICTS_AT_ONCE = 2`
    // across all off-autopilot boards).
    if is_judge_kind(cand.kind) {
        let board_judges = state
            .active_judges_by_board
            .get(&board.slug)
            .copied()
            .unwrap_or(0);
        if board_judges >= JUDGES_AT_ONCE {
            return Err((
                DispatchHoldReason::JudgeCap,
                format!(
                    "board {} at judge cap ({board_judges}/{JUDGES_AT_ONCE})",
                    board.slug
                ),
                None,
            ));
        }
        if cand.kind == RunKind::Verdict
            && !board.autopilot
            && state.off_autopilot_verdicts >= HOME_VERDICTS_AT_ONCE
        {
            return Err((
                DispatchHoldReason::JudgeCap,
                format!(
                    "off-autopilot verdict cap reached ({}/{HOME_VERDICTS_AT_ONCE})",
                    state.off_autopilot_verdicts
                ),
                None,
            ));
        }
    }

    // Fleet-wide `conflict_keys` capacity.
    for spec in &cand.conflict_keys {
        if let Some(&(holders, existing_max)) = state.conflict_locks.get(&spec.key) {
            let effective_max = existing_max.min(spec.max);
            if holders >= effective_max {
                return Err((
                    DispatchHoldReason::ConflictKey,
                    format!(
                        "conflict key {:?} at capacity ({holders}/{effective_max})",
                        spec.key
                    ),
                    Some(spec.key.clone()),
                ));
            }
        }
    }

    Ok(())
}

fn toleration_matches(tol: &TaintToleration, taint: &Taint) -> bool {
    if tol.key != taint.key {
        return false;
    }
    if let Some(effect) = tol.effect
        && effect != taint.effect
    {
        return false;
    }
    if let Some(ref val) = tol.value
        && taint.value.as_deref() != Some(val.as_str())
    {
        return false;
    }
    true
}

pub(crate) fn is_tolerated(taint: &Taint, tolerations: &[TaintToleration]) -> bool {
    tolerations.iter().any(|tol| toleration_matches(tol, taint))
}

/// Stage 3 — Kubernetes-style predicates (taints, capabilities, runner, repo).
/// Returns the selected runner name on match.
fn filter_candidate(
    cand: &Candidate,
    taints: &[Taint],
    capabilities: &[String],
    runners: &[String],
    repos: &[String],
) -> std::result::Result<String, (DispatchHoldReason, String)> {
    // 1. Hard `NoSchedule` taints.
    for taint in taints {
        if taint.effect == TaintEffect::NoSchedule && !is_tolerated(taint, &cand.tolerations) {
            let label = match &taint.value {
                Some(v) => format!("{}={v}:NoSchedule", taint.key),
                None => format!("{}:NoSchedule", taint.key),
            };
            return Err((
                DispatchHoldReason::Taint,
                format!("device taint {label} not tolerated"),
            ));
        }
    }

    // 2. Hard capability predicates (`requires`).
    for req_cap in &cand.requires {
        if !capabilities.contains(req_cap) {
            return Err((
                DispatchHoldReason::Requires,
                format!("device lacks required capability {req_cap:?}"),
            ));
        }
    }

    // 3. Runner match.
    let chosen_runner = match cand.kind {
        RunKind::Merge | RunKind::Housekeeping => {
            if runners.iter().any(|r| r == "deterministic") {
                "deterministic".to_owned()
            } else {
                return Err((
                    DispatchHoldReason::Runner,
                    "candidate requires runner \"deterministic\"".to_owned(),
                ));
            }
        }
        RunKind::Command => {
            let want = cand.preferred_runner.as_deref().unwrap_or("deterministic");
            if runners.iter().any(|r| r == want) {
                want.to_owned()
            } else {
                return Err((
                    DispatchHoldReason::Runner,
                    format!("command run requires runner {want:?}"),
                ));
            }
        }
        RunKind::Worker | RunKind::Review | RunKind::Verdict | RunKind::Triage | RunKind::Goal => {
            if let Some(ref pref) = cand.preferred_runner
                && runners.iter().any(|r| r == pref)
            {
                pref.clone()
            } else if let Some(runner) = runners.iter().find(|r| matches!(r.as_str(), "acp" | "t3"))
            {
                runner.clone()
            } else {
                return Err((
                    DispatchHoldReason::Runner,
                    "LLM run requires runner \"acp\" or \"t3\"".to_owned(),
                ));
            }
        }
        RunKind::Interactive | RunKind::Unknown => {
            return Err((
                DispatchHoldReason::Runner,
                "unsupported run kind".to_owned(),
            ));
        }
    };

    // 4. Repo availability for runs needing a local checkout.
    if cand.needs_local_repo
        && let Some(ref repo) = cand.repo
    {
        let has_warm = repos.iter().any(|r| r == repo);
        let cloneable = capabilities.iter().any(|c| c == "git:cloneable");
        if !has_warm && !cloneable {
            return Err((
                DispatchHoldReason::Repo,
                format!("device lacks repo {repo:?} and capability \"git:cloneable\""),
            ));
        }
    }

    Ok(chosen_runner)
}

pub(crate) fn kind_factor(kind: RunKind) -> f64 {
    match kind {
        RunKind::Merge => 1.00,
        RunKind::Housekeeping => 0.95,
        RunKind::Review => 0.80,
        RunKind::Verdict => 0.65,
        RunKind::Triage => 0.60,
        RunKind::Command => 0.55,
        RunKind::Worker => 0.40,
        RunKind::Goal => 0.20,
        RunKind::Interactive | RunKind::Unknown => 0.0,
    }
}

pub(crate) fn weight_label(weight: ResourceWeight) -> &'static str {
    match weight {
        ResourceWeight::Light => "light",
        ResourceWeight::Standard => "standard",
        ResourceWeight::Heavy => "heavy",
        ResourceWeight::Unknown => "standard",
    }
}

/// Pure inputs for [`score_candidate_base`], shared between live dispatch and offline replay.
pub(crate) struct ScoreInputs<'a> {
    pub now_ms: i64,
    pub kind: RunKind,
    pub priority: Priority,
    pub rank: Option<u32>,
    pub created_at_ms: i64,
    pub weight_tier: ResourceWeight,
    pub repo: Option<&'a str>,
    pub tolerations: &'a [TaintToleration],
    pub affinity: Option<&'a TaskAffinity>,
    pub board_home_devices: &'a [DeviceName],
    pub board_recent_units: u64,
    pub total_recent_units: u64,
    pub device_name: &'a DeviceName,
    pub device_preference: i32,
    pub device_taints: &'a [Taint],
    pub device_repos: &'a [String],
}

/// Pure multifactor scoring function (before backfill), shared by live dispatch and the
/// deterministic offline replay simulator so both evaluate identical weights and formulas.
pub(crate) fn score_candidate_base(
    input: &ScoreInputs<'_>,
    w: &SchedulerWeights,
) -> ScoreBreakdown {
    // 1. Kind priority (finish WIP before starting new work).
    let kind = w.w_kind * kind_factor(input.kind);

    // 2. Priority (`p1 > p2 > p3`).
    let prio_factor = match input.priority {
        Priority::P1 => 1.0,
        Priority::P2 => 0.5,
        Priority::P3 => 0.0,
    };
    let priority = w.w_prio * prio_factor;

    // 3. Explicit rank inside priority level (lower first; strictly smaller than priority gap).
    let rank = match input.rank {
        Some(r) => w.w_rank / (1.0 + 0.1 * f64::from(r)),
        None => 0.0,
    };

    // 4. Anti-starvation age ramp (smooth log boost from `created_at` via injectable clock).
    let age_ms = input.now_ms.saturating_sub(input.created_at_ms).max(0);
    let age_hours = (age_ms as f64) / 3_600_000.0;
    let age_norm = ((1.0 + age_hours).ln() / (1.0 + 168.0_f64).ln()).min(2.0);
    let age = w.w_age * age_norm;

    // 5. Fair-share penalty proportional to board's share of recent 24h consumption.
    let fair_share = if input.total_recent_units == 0 {
        0.0
    } else {
        let share = (input.board_recent_units as f64) / (input.total_recent_units as f64);
        -w.w_fair * share
    };

    // 6. Warm-repo locality bonus.
    let locality = match input.repo {
        Some(repo) if input.device_repos.iter().any(|r| r == repo) => {
            let mult = if input.affinity.is_some_and(|a| a.prefer_warm_repo) {
                1.25
            } else {
                1.0
            };
            w.w_loc * mult
        }
        _ => 0.0,
    };

    // 7. Device preference, task affinity, ordered `board.home_devices`, and
    //    `PreferNoSchedule` soft taint penalty.
    let mut pref = (f64::from(input.device_preference) * 0.05).clamp(-10.0, 10.0);
    if let Some(aff) = input.affinity {
        if aff.prefer_device.as_ref() == Some(input.device_name) {
            pref += 6.0;
        } else if let Some(idx) = aff
            .preferred_devices
            .iter()
            .position(|d| d == input.device_name)
        {
            pref += 4.0 / (1.0 + idx as f64);
        }
    }
    if let Some(idx) = input
        .board_home_devices
        .iter()
        .position(|d| d == input.device_name)
    {
        pref += 3.0 / (1.0 + idx as f64);
    }
    let cand_weight = weight_label(input.weight_tier);
    for taint in input.device_taints {
        if taint.effect != TaintEffect::PreferNoSchedule {
            continue;
        }
        if is_tolerated(taint, input.tolerations) {
            continue;
        }
        // `weight=<tier>:PreferNoSchedule` applies only when the candidate matches `<tier>`.
        if taint.key == "weight"
            && let Some(ref val) = taint.value
            && val != cand_weight
        {
            continue;
        }
        pref -= 10.0;
    }
    let device_pref = w.w_pref * pref;

    ScoreBreakdown {
        kind,
        priority,
        rank,
        age,
        fair_share,
        locality,
        device_pref,
        backfill: 0.0,
    }
}

/// Stage 4 — Multifactor scoring (before backfill).
#[allow(clippy::too_many_arguments)]
fn compute_base_score(
    cx: &Ctx<'_>,
    cand: &Candidate,
    board: &Board,
    device: &DeviceEntry,
    taints: &[Taint],
    repos: &[String],
    state: &SchedulerState,
    w: &SchedulerWeights,
) -> ScoreBreakdown {
    let board_recent_units = state
        .recent_board_units
        .get(&cand.board)
        .copied()
        .unwrap_or(0);
    score_candidate_base(
        &ScoreInputs {
            now_ms: cx.now_ms(),
            kind: cand.kind,
            priority: cand.priority,
            rank: cand.rank,
            created_at_ms: cand.created_at.timestamp_millis(),
            weight_tier: cand.weight_tier,
            repo: cand.repo.as_deref(),
            tolerations: &cand.tolerations,
            affinity: cand.affinity.as_ref(),
            board_home_devices: &board.home_devices,
            board_recent_units,
            total_recent_units: state.total_recent_units,
            device_name: &device.name,
            device_preference: device.preference,
            device_taints: taints,
            device_repos: repos,
        },
        w,
    )
}

/// Stage 5 — Atomic Grant: inserts or updates the `Run` in `leased` state, acquires all
/// `conflict_locks`, and emits a Trail Rule event with score breakdown and cause.
fn grant_candidate(
    cx: &mut Ctx<'_>,
    actor: &str,
    device: &DeviceName,
    cand: &Candidate,
    chosen_runner: &str,
    breakdown: ScoreBreakdown,
    policy: &SchedulerPolicy,
) -> Result<ClaimedRun> {
    let now = cx.now_ms();
    let lease_expires_at = expires(cx, crate::ops::policy::effective_lease_ms(cx, policy));
    let score = breakdown.total();
    let policy_version = policy.version.as_str();

    let (run_id, event_type, cleared_agent) =
        if let Some(ref existing_id) = cand.existing_queued_run {
            // Clear any cross-device `dispatch:<other>` agent_id before reassigning device so the
            // run never violates the device/dispatch-agent invariant.
            let existing_agent: Option<String> = cx
                .db
                .prepare_cached("SELECT agent_id FROM runs WHERE id = ?1 AND state = 'queued'")?
                .query_row([existing_id.as_str()], |r| r.get(0))
                .optional()?
                .flatten();
            let clear_agent = existing_agent
                .as_deref()
                .and_then(|s| s.parse::<AgentId>().ok())
                .and_then(|a| a.dispatch_device())
                .is_some_and(|d| d != *device);
            let updated = cx.db.execute(
                "UPDATE runs SET state = 'leased', device = ?2, runner = ?3, \
             agent_id = CASE WHEN ?4 THEN NULL ELSE agent_id END, \
             lease_expires_at = ?5, policy_version = ?6, updated_at = ?7 \
             WHERE id = ?1 AND state = 'queued'",
                params![
                    existing_id.as_str(),
                    device.as_str(),
                    chosen_runner,
                    clear_agent,
                    lease_expires_at,
                    policy_version,
                    now,
                ],
            )?;
            if updated == 0 {
                return Err(crate::error::refused(
                    format!("queued run {existing_id} was already claimed"),
                    "retry POST /v1/dispatch/claim",
                ));
            }
            if !clear_agent
                && let Some(ref agent_raw) = existing_agent
                && let Ok(agent_id) = agent_raw.parse::<AgentId>()
            {
                crate::ops::runs::absorb_auto_interactive_runs(cx, existing_id, &agent_id)?;
            }
            (existing_id.clone(), EventType::RunUpdated, clear_agent)
        } else {
            let id = new_run_id(cx)?;
            cx.db.execute(
                "INSERT INTO runs ( \
                 id, task_id, board, kind, origin, device, runner, agent_id, state, \
                 lease_expires_at, worktree_branch, cwd, title, attach_hint, cost_units, \
                 policy_version, created_at, started_at, ended_at, updated_at \
             ) VALUES ( \
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 'leased', \
                 ?8, NULL, NULL, ?9, NULL, ?10, \
                 ?11, ?12, NULL, NULL, ?12 \
             )",
                params![
                    id.as_str(),
                    cand.task_id.as_ref().map(TaskId::as_str),
                    cand.board.as_str(),
                    enum_str(&cand.kind),
                    enum_str(&RunOrigin::Managed),
                    device.as_str(),
                    chosen_runner,
                    lease_expires_at,
                    cand.title,
                    cand.cost_units,
                    policy_version,
                    now,
                ],
            )?;
            (id, EventType::RunCreated, false)
        };

    let acquired = acquire_conflict_locks(cx, &run_id, &cand.conflict_keys)?;

    let mut payload = serde_json::json!({
        "run_id": run_id,
        "kind": cand.kind,
        "origin": RunOrigin::Managed,
        "device": device,
        "runner": chosen_runner,
        "state": RunState::Leased,
        "cost_units": cand.cost_units,
        "lease_expires_at": ts(lease_expires_at),
        "policy_version": policy_version,
        "cause": "dispatch_claim",
        "score": score,
        "score_breakdown": breakdown,
    });
    if cand.existing_queued_run.is_some() {
        payload["from"] = serde_json::json!(RunState::Queued);
        payload["to"] = serde_json::json!(RunState::Leased);
        let mut fields = vec!["state", "device", "runner"];
        if cleared_agent {
            fields.push("agent_id");
        }
        payload["fields"] = serde_json::json!(fields);
    }
    if !acquired.is_empty() {
        payload["conflict_keys"] = serde_json::json!(acquired);
    }
    if !cand.tasks.is_empty() {
        payload["tasks"] = serde_json::json!(cand.tasks);
    }
    if let Some(ref action) = cand.housekeeping_action {
        payload["housekeeping_action"] = serde_json::json!(action);
    }
    cx.emit(
        Some(&cand.board),
        actor,
        event_type,
        cand.task_id.as_ref(),
        payload,
    )?;

    let run = get_run(cx, &run_id)?;
    Ok(ClaimedRun {
        run,
        board: cand.board.clone(),
        task_id: cand.task_id.clone(),
        tasks: cand.tasks.clone(),
        score,
        score_breakdown: breakdown,
        conflict_keys: cand.conflict_keys.clone(),
        housekeeping_action: cand.housekeeping_action.clone(),
    })
}
