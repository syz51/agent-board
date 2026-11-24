//! Automated continuous scheduler evaluation and guardrailed self-tuning
//! (`docs/SCHEDULER_DESIGN.md`, Section 6.4).
//!
//! Runs periodically in the server sweeper (gated by `[scheduler.autotune]`) and on demand
//! via `POST /v1/scheduler/evaluations/run`. Uses `ops::replay::replay_schedule_on_conn`
//! in-process against the live database connection so candidate policies are evaluated with
//! the exact deterministic simulator without temp files or subprocesses.

use board_proto::{
    AgentId, AutotuneDecision, AutotuneMode, Board, BoardSlug, CreateBoard, EventType,
    FEEDBACK_SIGNATURE_MAX, ReplayMetrics, ReplayPolicyConfig, SCHEDULER_EVALUATIONS_LIMIT_DEFAULT,
    SCHEDULER_EVALUATIONS_LIMIT_MAX, SCHEDULER_POLICY_VERSION_DEFAULT, SchedulerEvaluation,
    SchedulerEvaluationsResponse, SchedulerPolicy, SchedulerWeights, TaskId,
};
use rusqlite::{OptionalExtension, Row, params};
use sha2::{Digest, Sha256};

use crate::auth::hex;
use crate::config::AutotuneConfig;
use crate::db::{Ctx, enum_str, parse_enum, parse_id, parse_json, ts};
use crate::error::{Result, invalid};
use crate::ops::boards;
use crate::ops::feedback::file_signal;
use crate::ops::policy::{active as active_policy, validate_weights};
use crate::ops::replay::{ReplayOptions, replay_schedule_on_conn};
use crate::ops::stats::percentile_of;
use crate::store::find_board;
use crate::text::cut;

const DAY_MS: i64 = 86_400_000;
const ROLLBACK_SIGNAL_COOLDOWN_MS: i64 = 3_600_000;

fn server_reporter() -> AgentId {
    "dispatch:server"
        .parse()
        .expect("dispatch:server is a valid AgentId")
}

fn bounded_signature(prefix: &str, suffix: &str) -> String {
    let raw = format!("{prefix}:{suffix}");
    if raw.chars().count() <= FEEDBACK_SIGNATURE_MAX {
        return raw;
    }
    let digest = hex(&Sha256::digest(raw.as_bytes()));
    let keep = FEEDBACK_SIGNATURE_MAX.saturating_sub(13);
    let head: String = raw.chars().take(keep).collect();
    format!("{head}:{}", &digest[..12])
}

/// Validates `?limit=` for `GET /v1/scheduler/evaluations`.
pub(crate) fn validate_limit(limit: Option<u32>) -> Result<u32> {
    match limit.unwrap_or(SCHEDULER_EVALUATIONS_LIMIT_DEFAULT) {
        n @ 1..=SCHEDULER_EVALUATIONS_LIMIT_MAX => Ok(n),
        n => Err(invalid(
            format!("limit={n} is out of range"),
            format!("send limit between 1 and {SCHEDULER_EVALUATIONS_LIMIT_MAX}"),
        )),
    }
}

const EVAL_COLUMNS: &str = "id, evaluated_at, mode, decision, baseline_policy_version, \
     candidate_policy_version, applied_policy_version, previous_policy_version, window_days, \
     sample_runs, sample_tasks, baseline_metrics_json, candidate_metrics_json, \
     candidate_weights_json, score_delta_pct, reason, feedback_task_id";

fn evaluation_row(r: &Row<'_>) -> rusqlite::Result<SchedulerEvaluation> {
    let baseline_metrics: Option<ReplayMetrics> = r
        .get::<_, Option<String>>(11)?
        .map(|s| parse_json(&s))
        .transpose()?;
    let candidate_metrics: Option<ReplayMetrics> = r
        .get::<_, Option<String>>(12)?
        .map(|s| parse_json(&s))
        .transpose()?;
    let candidate_weights: Option<SchedulerWeights> = r
        .get::<_, Option<String>>(13)?
        .map(|s| parse_json(&s))
        .transpose()?;
    let feedback_task_id: Option<TaskId> = r
        .get::<_, Option<String>>(16)?
        .map(|s| parse_id(&s))
        .transpose()?;
    Ok(SchedulerEvaluation {
        id: r.get(0)?,
        evaluated_at: ts(r.get(1)?),
        mode: parse_enum(&r.get::<_, String>(2)?)?,
        decision: parse_enum(&r.get::<_, String>(3)?)?,
        baseline_policy_version: r.get(4)?,
        candidate_policy_version: r.get(5)?,
        applied_policy_version: r.get(6)?,
        previous_policy_version: r.get(7)?,
        window_days: u32::try_from(r.get::<_, i64>(8)?.max(1)).unwrap_or(7),
        sample_runs: u32::try_from(r.get::<_, i64>(9)?.max(0)).unwrap_or(0),
        sample_tasks: u32::try_from(r.get::<_, i64>(10)?.max(0)).unwrap_or(0),
        baseline_metrics,
        candidate_metrics,
        candidate_weights,
        score_delta_pct: r.get(14)?,
        reason: r.get(15)?,
        feedback_task_id,
    })
}

/// `GET /v1/scheduler/evaluations?limit=20`.
pub(crate) fn list_evaluations(cx: &Ctx<'_>, limit: u32) -> Result<SchedulerEvaluationsResponse> {
    let cfg = &cx.s.scheduler_autotune;
    let evaluations = cx
        .db
        .prepare_cached(&format!(
            "SELECT {EVAL_COLUMNS} FROM scheduler_evaluations \
             ORDER BY evaluated_at DESC, id DESC LIMIT ?1"
        ))?
        .query_map([i64::from(limit)], evaluation_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(SchedulerEvaluationsResponse {
        mode: cfg.mode,
        enabled: cfg.enabled,
        interval_secs: cfg.interval_secs,
        evaluations,
    })
}

/// Returns the most recent [`SchedulerEvaluation`] row, if any, for `GET /v1/stats/scheduler`.
pub(crate) fn latest_evaluation(cx: &Ctx<'_>) -> Result<Option<SchedulerEvaluation>> {
    Ok(cx
        .db
        .prepare_cached(&format!(
            "SELECT {EVAL_COLUMNS} FROM scheduler_evaluations \
             ORDER BY evaluated_at DESC, id DESC LIMIT 1"
        ))?
        .query_row([], evaluation_row)
        .optional()?)
}

/// Periodic background sweeper hook. Respects `enabled`, `mode != Off`, and `interval_secs`
/// against the injectable server clock (`cx.now`). Returns `Ok(None)` when skipped by gate.
pub(crate) fn sweep_autotune(cx: &mut Ctx<'_>) -> Result<Option<SchedulerEvaluation>> {
    let cfg = cx.s.scheduler_autotune.clone();
    if !cfg.enabled || matches!(cfg.mode, AutotuneMode::Off | AutotuneMode::Unknown) {
        return Ok(None);
    }
    let last_at: Option<i64> = cx
        .db
        .prepare_cached("SELECT MAX(evaluated_at) FROM scheduler_evaluations")?
        .query_row([], |r| r.get(0))?;
    if let Some(last) = last_at
        && cx.now_ms().saturating_sub(last) < cfg.interval_ms()
    {
        return Ok(None);
    }
    Ok(Some(evaluate_and_tune(cx, &cfg, "server")?))
}

/// Admin trigger (`POST /v1/scheduler/evaluations/run`): bypasses the `enabled` and
/// `interval_secs` gates and runs one evaluation cycle immediately under the configured `mode`.
pub(crate) fn run_now(cx: &mut Ctx<'_>, actor: &str) -> Result<SchedulerEvaluation> {
    let cfg = cx.s.scheduler_autotune.clone();
    evaluate_and_tune(cx, &cfg, actor)
}

struct ActivePolicyMeta {
    policy: SchedulerPolicy,
    auto_promoted: bool,
    previous_version: Option<String>,
}

fn load_active_policy_meta(cx: &Ctx<'_>) -> Result<ActivePolicyMeta> {
    let policy = active_policy(cx)?;
    let meta: Option<(i64, Option<String>)> = cx
        .db
        .prepare_cached(
            "SELECT auto_promoted, previous_version FROM scheduler_policies \
             WHERE version = ?1 LIMIT 1",
        )?
        .query_row([policy.version.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let (auto_promoted, previous_version) = match meta {
        Some((flag, prev)) => (flag != 0, prev),
        None => (false, None),
    };
    Ok(ActivePolicyMeta {
        policy,
        auto_promoted,
        previous_version,
    })
}

struct NewEvaluation<'a> {
    mode: AutotuneMode,
    decision: AutotuneDecision,
    baseline_policy_version: &'a str,
    candidate_policy_version: Option<&'a str>,
    applied_policy_version: Option<&'a str>,
    previous_policy_version: Option<&'a str>,
    window_days: u32,
    sample_runs: u32,
    sample_tasks: u32,
    baseline_metrics: Option<&'a ReplayMetrics>,
    candidate_metrics: Option<&'a ReplayMetrics>,
    candidate_weights: Option<&'a SchedulerWeights>,
    score_delta_pct: Option<f64>,
    reason: &'a str,
    feedback_task_id: Option<&'a TaskId>,
}

fn record_evaluation(cx: &Ctx<'_>, rec: NewEvaluation<'_>) -> Result<SchedulerEvaluation> {
    let now = cx.now_ms();
    let baseline_json = rec
        .baseline_metrics
        .map(|m| serde_json::to_string(m).expect("replay metrics serialize"));
    let candidate_json = rec
        .candidate_metrics
        .map(|m| serde_json::to_string(m).expect("replay metrics serialize"));
    let weights_json = rec
        .candidate_weights
        .map(|w| serde_json::to_string(w).expect("weights serialize"));
    cx.db.execute(
        "INSERT INTO scheduler_evaluations ( \
             evaluated_at, mode, decision, baseline_policy_version, candidate_policy_version, \
             applied_policy_version, previous_policy_version, window_days, sample_runs, \
             sample_tasks, baseline_metrics_json, candidate_metrics_json, candidate_weights_json, \
             score_delta_pct, reason, feedback_task_id \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            now,
            enum_str(&rec.mode),
            enum_str(&rec.decision),
            rec.baseline_policy_version,
            rec.candidate_policy_version,
            rec.applied_policy_version,
            rec.previous_policy_version,
            i64::from(rec.window_days),
            i64::from(rec.sample_runs),
            i64::from(rec.sample_tasks),
            baseline_json,
            candidate_json,
            weights_json,
            rec.score_delta_pct,
            rec.reason,
            rec.feedback_task_id.map(TaskId::as_str),
        ],
    )?;
    let id = cx.db.last_insert_rowid();
    Ok(cx.db.query_row(
        &format!("SELECT {EVAL_COLUMNS} FROM scheduler_evaluations WHERE id = ?1"),
        [id],
        evaluation_row,
    )?)
}

fn evaluate_and_tune(
    cx: &mut Ctx<'_>,
    cfg: &AutotuneConfig,
    actor: &str,
) -> Result<SchedulerEvaluation> {
    let active = load_active_policy_meta(cx)?;

    // Step 1: Auto-Rollback Watchdog for auto-promoted policies.
    if !matches!(cfg.mode, AutotuneMode::Off | AutotuneMode::Unknown)
        && let Some(rolled_back) = maybe_rollback(cx, cfg, &active, actor)?
    {
        return Ok(rolled_back);
    }

    // Step 2: Sample-size gate over `[now - window_days, now]`.
    let until_ms = cx.now_ms();
    let since_ms = until_ms.saturating_sub(i64::from(cfg.window_days) * DAY_MS);
    let sample_runs: u32 = cx.db.query_row(
        "SELECT COUNT(*) FROM run_outcomes WHERE recorded_at >= ?1 AND recorded_at <= ?2",
        params![since_ms, until_ms],
        |r| r.get(0),
    )?;
    let sample_tasks: u32 = cx.db.query_row(
        "SELECT COUNT(*) FROM tasks WHERE auto = 1 AND created_at >= ?1 AND created_at <= ?2",
        params![since_ms, until_ms],
        |r| r.get(0),
    )?;

    if sample_runs < cfg.min_runs {
        let reason = format!(
            "insufficient sample: {sample_runs} run_outcomes in last {}d (min_runs={})",
            cfg.window_days, cfg.min_runs
        );
        return record_evaluation(
            cx,
            NewEvaluation {
                mode: cfg.mode,
                decision: AutotuneDecision::SkippedInsufficientData,
                baseline_policy_version: &active.policy.version,
                candidate_policy_version: None,
                applied_policy_version: None,
                previous_policy_version: None,
                window_days: cfg.window_days,
                sample_runs,
                sample_tasks,
                baseline_metrics: None,
                candidate_metrics: None,
                candidate_weights: None,
                score_delta_pct: None,
                reason: &reason,
                feedback_task_id: None,
            },
        );
    }

    // Deterministic bounded candidate search using the in-process replay simulator.
    let candidates = generate_candidates(&active.policy.weights)?;
    let mut baseline_metrics: Option<ReplayMetrics> = None;
    let mut replay_tasks = sample_tasks;
    let mut best_eligible: Option<EvaluatedCandidate> = None;
    let mut best_overall: Option<EvaluatedCandidate> = None;

    for (name, weights) in candidates {
        let cmp = replay_schedule_on_conn(
            cx.db,
            ReplayOptions {
                since: Some(ts(since_ms)),
                until: cx.now,
                days: cfg.window_days,
                candidate: Some(ReplayPolicyConfig {
                    version: name.to_owned(),
                    weights,
                    lease_ttl_secs: None,
                    sticky_grace_secs: None,
                }),
            },
        )?;
        replay_tasks = cmp.historical_tasks.max(sample_tasks);
        let base = baseline_metrics
            .get_or_insert_with(|| cmp.baseline.clone())
            .clone();
        let Some(cand_metrics) = cmp.candidate else {
            continue;
        };

        let eval = score_candidate(name, weights, &base, cand_metrics);
        if best_overall
            .as_ref()
            .is_none_or(|b| eval.score_delta_pct > b.score_delta_pct)
        {
            best_overall = Some(eval.clone());
        }
        if eval.guardrails_passed
            && best_eligible
                .as_ref()
                .is_none_or(|b| eval.score_delta_pct > b.score_delta_pct)
        {
            best_eligible = Some(eval);
        }
    }

    let baseline = match baseline_metrics {
        Some(b) => b,
        None => {
            let cmp = replay_schedule_on_conn(
                cx.db,
                ReplayOptions {
                    since: Some(ts(since_ms)),
                    until: cx.now,
                    days: cfg.window_days,
                    candidate: None,
                },
            )?;
            replay_tasks = cmp.historical_tasks.max(sample_tasks);
            cmp.baseline
        }
    };

    let Some(winner) = best_eligible else {
        let reason = match best_overall.as_ref() {
            Some(c) => format!(
                "no candidate passed no-regression guardrails (best rejected: {} delta {:+.1}%: {})",
                c.name,
                c.score_delta_pct,
                c.rejection_reason
                    .as_deref()
                    .unwrap_or("guardrail violation")
            ),
            None => "no bounded candidates differed from active policy weights".to_owned(),
        };
        return record_evaluation(
            cx,
            NewEvaluation {
                mode: cfg.mode,
                decision: AutotuneDecision::Keep,
                baseline_policy_version: &active.policy.version,
                candidate_policy_version: best_overall.as_ref().map(|c| c.name),
                applied_policy_version: None,
                previous_policy_version: None,
                window_days: cfg.window_days,
                sample_runs,
                sample_tasks: replay_tasks,
                baseline_metrics: Some(&baseline),
                candidate_metrics: best_overall.as_ref().map(|c| &c.metrics),
                candidate_weights: best_overall.as_ref().map(|c| &c.weights),
                score_delta_pct: best_overall.as_ref().map(|c| c.score_delta_pct),
                reason: &reason,
                feedback_task_id: None,
            },
        );
    };

    if winner.score_delta_pct < cfg.min_improvement_pct {
        let reason = format!(
            "best candidate {} improved {:+.1}% < threshold {:.1}% (p90 {} -> {}, gini {:.3} -> {:.3})",
            winner.name,
            winner.score_delta_pct,
            cfg.min_improvement_pct,
            fmt_opt_secs(baseline.p90_wait_secs),
            fmt_opt_secs(winner.metrics.p90_wait_secs),
            baseline.fair_share_gini,
            winner.metrics.fair_share_gini,
        );
        return record_evaluation(
            cx,
            NewEvaluation {
                mode: cfg.mode,
                decision: AutotuneDecision::Keep,
                baseline_policy_version: &active.policy.version,
                candidate_policy_version: Some(winner.name),
                applied_policy_version: None,
                previous_policy_version: None,
                window_days: cfg.window_days,
                sample_runs,
                sample_tasks: replay_tasks,
                baseline_metrics: Some(&baseline),
                candidate_metrics: Some(&winner.metrics),
                candidate_weights: Some(&winner.weights),
                score_delta_pct: Some(winner.score_delta_pct),
                reason: &reason,
                feedback_task_id: None,
            },
        );
    }

    // Step 3: Cooldown gate since last promotion, proposal, or rollback.
    let last_change: Option<i64> = cx
        .db
        .prepare_cached(
            "SELECT MAX(evaluated_at) FROM scheduler_evaluations \
             WHERE decision IN ('promoted', 'proposed', 'rolled_back')",
        )?
        .query_row([], |r| r.get(0))?;
    if let Some(last) = last_change {
        let elapsed_ms = until_ms.saturating_sub(last);
        if elapsed_ms < cfg.cooldown_ms() {
            let remaining_secs = (cfg.cooldown_ms() - elapsed_ms) / 1_000;
            let reason = format!(
                "candidate {} improved {:+.1}% (>= {:.1}%), held by cooldown ({remaining_secs}s remaining)",
                winner.name, winner.score_delta_pct, cfg.min_improvement_pct
            );
            return record_evaluation(
                cx,
                NewEvaluation {
                    mode: cfg.mode,
                    decision: AutotuneDecision::Keep,
                    baseline_policy_version: &active.policy.version,
                    candidate_policy_version: Some(winner.name),
                    applied_policy_version: None,
                    previous_policy_version: None,
                    window_days: cfg.window_days,
                    sample_runs,
                    sample_tasks: replay_tasks,
                    baseline_metrics: Some(&baseline),
                    candidate_metrics: Some(&winner.metrics),
                    candidate_weights: Some(&winner.weights),
                    score_delta_pct: Some(winner.score_delta_pct),
                    reason: &reason,
                    feedback_task_id: None,
                },
            );
        }
    }

    match cfg.mode {
        AutotuneMode::Off | AutotuneMode::Unknown => {
            let reason = format!(
                "mode is off; best candidate {} improved {:+.1}% (no action taken)",
                winner.name, winner.score_delta_pct
            );
            record_evaluation(
                cx,
                NewEvaluation {
                    mode: cfg.mode,
                    decision: AutotuneDecision::Keep,
                    baseline_policy_version: &active.policy.version,
                    candidate_policy_version: Some(winner.name),
                    applied_policy_version: None,
                    previous_policy_version: None,
                    window_days: cfg.window_days,
                    sample_runs,
                    sample_tasks: replay_tasks,
                    baseline_metrics: Some(&baseline),
                    candidate_metrics: Some(&winner.metrics),
                    candidate_weights: Some(&winner.weights),
                    score_delta_pct: Some(winner.score_delta_pct),
                    reason: &reason,
                    feedback_task_id: None,
                },
            )
        }
        AutotuneMode::Apply => {
            let new_version = next_auto_version(cx)?;
            let notes = format!(
                "Auto-promoted {} ({:+.1}% composite replay gain over {} across {} runs/{} tasks in {}d; p90 {} -> {}, gini {:.3} -> {:.3})",
                winner.name,
                winner.score_delta_pct,
                active.policy.version,
                sample_runs,
                replay_tasks,
                cfg.window_days,
                fmt_opt_secs(baseline.p90_wait_secs),
                fmt_opt_secs(winner.metrics.p90_wait_secs),
                baseline.fair_share_gini,
                winner.metrics.fair_share_gini,
            );
            let weights_json = serde_json::to_string(&winner.weights).expect("weights serialize");
            cx.db.execute(
                "UPDATE scheduler_policies SET active = 0 WHERE active = 1",
                [],
            )?;
            cx.db.execute(
                "INSERT INTO scheduler_policies ( \
                     version, weights_json, lease_ttl_secs, sticky_grace_secs, \
                     notes, active, created_by, created_at, auto_promoted, previous_version \
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 1, 'server:autotune', ?6, 1, ?7)",
                params![
                    new_version,
                    weights_json,
                    i64::try_from(active.policy.lease_ttl_secs).unwrap_or(180),
                    i64::try_from(active.policy.sticky_grace_secs).unwrap_or(600),
                    notes,
                    until_ms,
                    active.policy.version,
                ],
            )?;
            cx.emit(
                None,
                actor,
                EventType::SchedulerPolicyUpdated,
                None,
                serde_json::json!({
                    "action": "auto_promoted",
                    "version": new_version,
                    "previous_version": active.policy.version,
                    "candidate": winner.name,
                    "weights": winner.weights,
                    "lease_ttl_secs": active.policy.lease_ttl_secs,
                    "sticky_grace_secs": active.policy.sticky_grace_secs,
                    "score_delta_pct": winner.score_delta_pct,
                    "notes": notes,
                }),
            )?;
            let mut promoted_metrics = winner.metrics.clone();
            promoted_metrics.policy_version = new_version.clone();
            record_evaluation(
                cx,
                NewEvaluation {
                    mode: cfg.mode,
                    decision: AutotuneDecision::Promoted,
                    baseline_policy_version: &active.policy.version,
                    candidate_policy_version: Some(&new_version),
                    applied_policy_version: Some(&new_version),
                    previous_policy_version: Some(&active.policy.version),
                    window_days: cfg.window_days,
                    sample_runs,
                    sample_tasks: replay_tasks,
                    baseline_metrics: Some(&baseline),
                    candidate_metrics: Some(&promoted_metrics),
                    candidate_weights: Some(&winner.weights),
                    score_delta_pct: Some(winner.score_delta_pct),
                    reason: &notes,
                    feedback_task_id: None,
                },
            )
        }
        AutotuneMode::Propose => {
            let proposed_version = next_auto_version(cx)?;
            let task_id = file_proposal_task(
                cx,
                actor,
                &active.policy.version,
                &proposed_version,
                &winner,
                &baseline,
                cfg.window_days,
                sample_runs,
                replay_tasks,
            )?;
            let reason = format!(
                "proposed {} ({:+.1}% replay improvement over {}; filed {task_id})",
                winner.name, winner.score_delta_pct, active.policy.version
            );
            record_evaluation(
                cx,
                NewEvaluation {
                    mode: cfg.mode,
                    decision: AutotuneDecision::Proposed,
                    baseline_policy_version: &active.policy.version,
                    candidate_policy_version: Some(&proposed_version),
                    applied_policy_version: None,
                    previous_policy_version: Some(&active.policy.version),
                    window_days: cfg.window_days,
                    sample_runs,
                    sample_tasks: replay_tasks,
                    baseline_metrics: Some(&baseline),
                    candidate_metrics: Some(&winner.metrics),
                    candidate_weights: Some(&winner.weights),
                    score_delta_pct: Some(winner.score_delta_pct),
                    reason: &reason,
                    feedback_task_id: Some(&task_id),
                },
            )
        }
    }
}

/// Step 1: Auto-Rollback Watchdog. Checks live post-promotion `run_outcomes` against the
/// pre-promotion baseline for `previous_version`; if `orphan_rate` or `ready_to_leased` p90
/// regressed past configured guardrails, reactivates `previous_version`, emits a Trail Rule
/// event, files a deduplicated diagnostic signal on `feedback_board`, and records `RolledBack`.
fn maybe_rollback(
    cx: &mut Ctx<'_>,
    cfg: &AutotuneConfig,
    active: &ActivePolicyMeta,
    actor: &str,
) -> Result<Option<SchedulerEvaluation>> {
    if !active.auto_promoted && !active.policy.version.contains("-auto-") {
        return Ok(None);
    }
    let Some(ref prev_version) = active.previous_version else {
        return Ok(None);
    };
    let promoted_at = active.policy.created_at.timestamp_millis();

    let live_rows: Vec<(String, Option<i64>)> = cx
        .db
        .prepare_cached(
            "SELECT terminal_state, ready_to_leased_secs FROM run_outcomes \
             WHERE policy_version = ?1 AND recorded_at >= ?2",
        )?
        .query_map(params![active.policy.version.as_str(), promoted_at], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let live_runs = live_rows.len() as u32;
    if live_runs < cfg.rollback_min_runs {
        return Ok(None);
    }

    let live_orphaned = live_rows.iter().filter(|(s, _)| s == "orphaned").count() as u32;
    let live_orphan_rate = f64::from(live_orphaned) / f64::from(live_runs);
    let live_waits: Vec<f64> = live_rows
        .iter()
        .filter_map(|(_, w)| w.map(|v| v as f64))
        .collect();
    let live_p90 = percentile_of(live_waits, 0.9);

    // Pre-promotion baseline from live `run_outcomes` of `prev_version` in the prior window.
    let base_from = promoted_at.saturating_sub(i64::from(cfg.window_days) * DAY_MS);
    let base_rows: Vec<(String, Option<i64>)> = cx
        .db
        .prepare_cached(
            "SELECT terminal_state, ready_to_leased_secs FROM run_outcomes \
             WHERE (policy_version = ?1 OR (?1 = ?4 AND policy_version IS NULL)) \
               AND recorded_at >= ?2 AND recorded_at < ?3",
        )?
        .query_map(
            params![
                prev_version.as_str(),
                base_from,
                promoted_at,
                SCHEDULER_POLICY_VERSION_DEFAULT
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .collect::<rusqlite::Result<_>>()?;
    let base_runs = base_rows.len() as u32;
    let base_orphan_rate = if base_runs > 0 {
        let base_orphaned = base_rows.iter().filter(|(s, _)| s == "orphaned").count() as u32;
        f64::from(base_orphaned) / f64::from(base_runs)
    } else {
        0.0
    };
    let base_waits: Vec<f64> = base_rows
        .iter()
        .filter_map(|(_, w)| w.map(|v| v as f64))
        .collect();
    let base_p90 = percentile_of(base_waits, 0.9).or_else(|| {
        promotion_baseline_p90(cx, &active.policy.version)
            .ok()
            .flatten()
    });

    let orphan_regressed = live_orphan_rate > base_orphan_rate + cfg.rollback_orphan_delta;
    let p90_regressed = matches!(
        (base_p90, live_p90),
        (Some(base), Some(live)) if base > 0.0 && live > base * cfg.rollback_p90_ratio
    );
    if !orphan_regressed && !p90_regressed {
        return Ok(None);
    }

    if !cx.exists(
        "SELECT 1 FROM scheduler_policies WHERE version = ?1",
        [prev_version.as_str()],
    )? {
        return Ok(None);
    }

    let mut triggers = Vec::new();
    if orphan_regressed {
        triggers.push(format!(
            "orphan_rate {:.1}% > baseline {:.1}% + {:.1}%",
            live_orphan_rate * 100.0,
            base_orphan_rate * 100.0,
            cfg.rollback_orphan_delta * 100.0
        ));
    }
    if p90_regressed && let (Some(base), Some(live)) = (base_p90, live_p90) {
        triggers.push(format!(
            "p90 wait {live:.1}s > baseline {base:.1}s * {:.2}",
            cfg.rollback_p90_ratio
        ));
    }
    let reason = format!(
        "auto-rolled back {} -> {} after {live_runs} live runs: {}",
        active.policy.version,
        prev_version,
        triggers.join("; ")
    );

    // Reactivate previous immutable policy snapshot.
    cx.db.execute(
        "UPDATE scheduler_policies SET active = 0 WHERE active = 1",
        [],
    )?;
    cx.db.execute(
        "UPDATE scheduler_policies SET active = 1 WHERE version = ?1",
        [prev_version.as_str()],
    )?;

    cx.emit(
        None,
        actor,
        EventType::SchedulerPolicyUpdated,
        None,
        serde_json::json!({
            "action": "auto_rolled_back",
            "version": prev_version,
            "previous_version": active.policy.version,
            "live_runs": live_runs,
            "live_orphan_rate": live_orphan_rate,
            "baseline_orphan_rate": base_orphan_rate,
            "live_p90_secs": live_p90,
            "baseline_p90_secs": base_p90,
            "reason": reason,
        }),
    )?;

    let signal_task_id = file_rollback_signal(cx, &active.policy.version, prev_version, &reason)?;
    let sample_tasks: u32 = cx.db.query_row(
        "SELECT COUNT(DISTINCT task_id) FROM runs \
         WHERE policy_version = ?1 AND created_at >= ?2 AND task_id IS NOT NULL",
        params![active.policy.version.as_str(), promoted_at],
        |r| r.get(0),
    )?;

    let eval = record_evaluation(
        cx,
        NewEvaluation {
            mode: cfg.mode,
            decision: AutotuneDecision::RolledBack,
            baseline_policy_version: &active.policy.version,
            candidate_policy_version: Some(prev_version),
            applied_policy_version: Some(prev_version),
            previous_policy_version: Some(&active.policy.version),
            window_days: cfg.window_days,
            sample_runs: live_runs,
            sample_tasks,
            baseline_metrics: None,
            candidate_metrics: None,
            candidate_weights: None,
            score_delta_pct: None,
            reason: &reason,
            feedback_task_id: signal_task_id.as_ref(),
        },
    )?;
    Ok(Some(eval))
}

fn promotion_baseline_p90(cx: &Ctx<'_>, promoted_version: &str) -> Result<Option<f64>> {
    let raw: Option<String> = cx
        .db
        .prepare_cached(
            "SELECT baseline_metrics_json FROM scheduler_evaluations \
             WHERE applied_policy_version = ?1 AND decision = 'promoted' \
             ORDER BY id DESC LIMIT 1",
        )?
        .query_row([promoted_version], |r| r.get(0))
        .optional()?
        .flatten();
    match raw {
        Some(json) => {
            let m: ReplayMetrics = parse_json(&json)?;
            Ok(m.p90_wait_secs)
        }
        None => Ok(None),
    }
}

fn file_rollback_signal(
    cx: &mut Ctx<'_>,
    rolled_from: &str,
    rolled_to: &str,
    reason: &str,
) -> Result<Option<TaskId>> {
    let now = cx.now_ms();
    let signature = bounded_signature("signal:scheduler:autotune_rollback", rolled_from);
    let existing: Option<Option<i64>> = cx
        .db
        .prepare_cached("SELECT last_filed_at FROM pathology_state WHERE signature = ?1")?
        .query_row([&signature], |r| r.get(0))
        .optional()?;
    if let Some(Some(last_filed)) = existing
        && now.saturating_sub(last_filed) < ROLLBACK_SIGNAL_COOLDOWN_MS
    {
        cx.db.execute(
            "UPDATE pathology_state SET last_seen_at = ?2 WHERE signature = ?1",
            params![signature, now],
        )?;
        let existing_task: Option<String> = cx
            .db
            .prepare_cached(
                "SELECT task FROM feedback WHERE signature = ?1 ORDER BY rowid DESC LIMIT 1",
            )?
            .query_row([&signature], |r| r.get(0))
            .optional()?;
        return existing_task.map(|s| Ok(parse_id(&s)?)).transpose();
    }

    let title = format!("scheduler: auto-rolled back {rolled_from} to {rolled_to}");
    let body = format!(
        "{title}\n\n{reason}\n\nInspect `GET /v1/scheduler/evaluations` and \
         `GET /v1/stats/scheduler` to review cohort performance before promoting another policy."
    );
    let (task_id, _) = file_signal(
        cx,
        "scheduler",
        &signature,
        &title,
        &body,
        reason,
        &server_reporter(),
    )?;
    cx.db.execute(
        "INSERT INTO pathology_state (signature, kind, first_seen_at, last_seen_at, last_filed_at) \
         VALUES (?1, 'autotune_rollback', ?2, ?2, ?2) \
         ON CONFLICT(signature) DO UPDATE SET \
             last_seen_at = excluded.last_seen_at, \
             last_filed_at = excluded.last_filed_at",
        params![signature, now],
    )?;
    Ok(Some(task_id))
}

#[derive(Debug, Clone)]
struct EvaluatedCandidate {
    name: &'static str,
    weights: SchedulerWeights,
    metrics: ReplayMetrics,
    score_delta_pct: f64,
    guardrails_passed: bool,
    rejection_reason: Option<String>,
}

fn clamp_weights(w: SchedulerWeights) -> SchedulerWeights {
    let r1 = |v: f64, lo: f64, hi: f64| (v.clamp(lo, hi) * 10.0).round() / 10.0;
    SchedulerWeights {
        w_kind: r1(w.w_kind, 50.0, 200.0),
        w_prio: r1(w.w_prio, 10.0, 80.0),
        w_rank: r1(w.w_rank, 2.0, 40.0),
        w_age: r1(w.w_age, 4.0, 64.0),
        w_fair: r1(w.w_fair, 4.0, 80.0),
        w_loc: r1(w.w_loc, 2.0, 40.0),
        w_pref: r1(w.w_pref, 0.5, 10.0),
        w_backfill: r1(w.w_backfill, 2.0, 32.0),
    }
}

fn generate_candidates(base: &SchedulerWeights) -> Result<Vec<(&'static str, SchedulerWeights)>> {
    let raw: [(&'static str, SchedulerWeights); 6] = [
        (
            "fair-boost",
            SchedulerWeights {
                w_fair: base.w_fair * 1.75 + 6.0,
                w_age: base.w_age * 1.15,
                ..*base
            },
        ),
        (
            "fair-strong",
            SchedulerWeights {
                w_fair: (base.w_fair * 3.0).max(40.0),
                w_age: (base.w_age * 1.5).max(24.0),
                ..*base
            },
        ),
        (
            "aging-boost",
            SchedulerWeights {
                w_age: base.w_age * 1.75 + 6.0,
                w_fair: base.w_fair * 1.2,
                ..*base
            },
        ),
        (
            "locality-boost",
            SchedulerWeights {
                w_loc: base.w_loc * 1.75 + 4.0,
                w_pref: base.w_pref * 1.25,
                ..*base
            },
        ),
        (
            "backfill-boost",
            SchedulerWeights {
                w_backfill: base.w_backfill * 1.75 + 4.0,
                w_age: base.w_age * 1.1,
                ..*base
            },
        ),
        (
            "balanced-fair-loc",
            SchedulerWeights {
                w_fair: base.w_fair * 1.5 + 4.0,
                w_age: base.w_age * 1.35 + 2.0,
                w_loc: base.w_loc * 1.4 + 2.0,
                w_backfill: base.w_backfill * 1.35 + 2.0,
                ..*base
            },
        ),
    ];

    let mut out = Vec::with_capacity(raw.len());
    for (name, w) in raw {
        let clamped = validate_weights(clamp_weights(w))?;
        if clamped != *base && !out.iter().any(|(_, existing)| *existing == clamped) {
            out.push((name, clamped));
        }
    }
    Ok(out)
}

fn score_candidate(
    name: &'static str,
    weights: SchedulerWeights,
    baseline: &ReplayMetrics,
    candidate: ReplayMetrics,
) -> EvaluatedCandidate {
    let mut rejections = Vec::new();
    if candidate.tasks_scheduled < baseline.tasks_scheduled {
        rejections.push(format!(
            "tasks_scheduled regressed ({} < {})",
            candidate.tasks_scheduled, baseline.tasks_scheduled
        ));
    }
    if candidate.conflict_blocks > baseline.conflict_blocks {
        rejections.push(format!(
            "conflict_blocks regressed ({} > {})",
            candidate.conflict_blocks, baseline.conflict_blocks
        ));
    }
    match (baseline.p50_wait_secs, candidate.p50_wait_secs) {
        (Some(b), Some(c)) if c > b * 1.05 => {
            rejections.push(format!("p50_wait regressed ({c:.1}s > {:.1}s)", b * 1.05));
        }
        (Some(b), None) => {
            rejections.push(format!("p50_wait missing (baseline {b:.1}s)"));
        }
        _ => {}
    }

    let p90_delta_pct = match (baseline.p90_wait_secs, candidate.p90_wait_secs) {
        (Some(b), Some(c)) if b > 0.0 => Some(((b - c) / b) * 100.0),
        (Some(b), Some(c)) if b == 0.0 && c > 0.0 => Some(-100.0),
        _ => None,
    };
    let gini_delta_pct = if baseline.fair_share_gini > 0.01 {
        Some(
            ((baseline.fair_share_gini - candidate.fair_share_gini) / baseline.fair_share_gini)
                * 100.0,
        )
    } else if candidate.fair_share_gini > baseline.fair_share_gini + 0.01 {
        Some(-(candidate.fair_share_gini - baseline.fair_share_gini) * 100.0)
    } else {
        None
    };

    if let Some(d) = p90_delta_pct
        && d < -10.0
    {
        rejections.push(format!("p90_wait regressed by {:.1}%", -d));
    }
    if let Some(d) = gini_delta_pct
        && d < -10.0
    {
        rejections.push(format!("fair_share_gini regressed by {:.1}%", -d));
    }

    let score_delta_pct = match (p90_delta_pct, gini_delta_pct) {
        (Some(p), Some(g)) => 0.5 * p + 0.5 * g,
        (Some(p), None) => p,
        (None, Some(g)) => g,
        (None, None) => 0.0,
    };
    let score_delta_pct = (score_delta_pct * 100.0).round() / 100.0;

    let guardrails_passed = rejections.is_empty();
    EvaluatedCandidate {
        name,
        weights,
        metrics: candidate,
        score_delta_pct,
        guardrails_passed,
        rejection_reason: (!rejections.is_empty()).then(|| rejections.join("; ")),
    }
}

fn next_auto_version(cx: &Ctx<'_>) -> Result<String> {
    let versions: Vec<String> = cx
        .db
        .prepare_cached("SELECT version FROM scheduler_policies")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut max_n: u64 = 1;
    for v in &versions {
        if let Some(rest) = v.strip_prefix('v') {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse::<u64>() {
                max_n = max_n.max(n);
            }
        }
    }
    let date = cx.now.format("%Y%m%d");
    let mut next = max_n + 1;
    loop {
        let candidate = format!("v{next}-auto-{date}");
        if !versions.iter().any(|v| v == &candidate) {
            return Ok(candidate);
        }
        next += 1;
    }
}

fn proposal_target_board(cx: &mut Ctx<'_>, actor: &str) -> Result<Board> {
    let agent_board_slug: BoardSlug = "agent-board".parse().expect("valid slug");
    if let Some(board) = find_board(cx, &agent_board_slug)?
        && board.status == board_proto::BoardStatus::Active
    {
        return Ok(board);
    }
    let slug = cx.s.feedback_board.clone();
    if let Some(board) = find_board(cx, &slug)? {
        return Ok(board);
    }
    boards::create(
        cx,
        actor,
        CreateBoard {
            slug,
            title: "Feedback".to_owned(),
            goal: "Reports about any setup and server-detected signals.".to_owned(),
            repos: Vec::new(),
            autopilot: false,
            home_devices: Vec::new(),
            temporary: false,
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn file_proposal_task(
    cx: &mut Ctx<'_>,
    actor: &str,
    baseline_version: &str,
    proposed_version: &str,
    winner: &EvaluatedCandidate,
    baseline: &ReplayMetrics,
    window_days: u32,
    sample_runs: u32,
    sample_tasks: u32,
) -> Result<TaskId> {
    let board = proposal_target_board(cx, actor)?;
    let number: i64 = cx.db.query_row(
        "UPDATE boards SET next_number = next_number + 1 WHERE slug = ?1 RETURNING next_number - 1",
        [board.slug.as_str()],
        |row| row.get(0),
    )?;
    let id = TaskId::new(&board.slug, number as u64);
    let now = cx.now_ms();
    let title = cut(
        &format!(
            "Tune scheduler policy to {proposed_version} ({:+.1}% replay gain)",
            winner.score_delta_pct
        ),
        cx.s.title_chars,
    );
    let weights_json = serde_json::to_string(&winner.weights).expect("weights serialize");
    let raw_body = format!(
        "Proposal: promote candidate `{}` as `{proposed_version}` (composite replay gain {:+.1}% over `{baseline_version}` across {sample_runs} runs / {sample_tasks} tasks in {window_days}d).\n\n\
         | Metric | Baseline (`{baseline_version}`) | Candidate (`{}`) |\n\
         |---|---|---|\n\
         | tasks_scheduled | {} | {} |\n\
         | p50_wait_secs | {} | {} |\n\
         | p90_wait_secs | {} | {} |\n\
         | fair_share_gini | {:.4} | {:.4} |\n\
         | conflict_blocks | {} | {} |\n\
         | backfill_count | {} | {} |\n\
         | locality_hits | {} | {} |\n\n\
         Candidate weights: `{weights_json}`\n\n\
         Change: apply via `PUT /v1/scheduler/policy` with `version = \"{proposed_version}\"`. Size S.",
        winner.name,
        winner.score_delta_pct,
        winner.name,
        baseline.tasks_scheduled,
        winner.metrics.tasks_scheduled,
        fmt_opt_secs(baseline.p50_wait_secs),
        fmt_opt_secs(winner.metrics.p50_wait_secs),
        fmt_opt_secs(baseline.p90_wait_secs),
        fmt_opt_secs(winner.metrics.p90_wait_secs),
        baseline.fair_share_gini,
        winner.metrics.fair_share_gini,
        baseline.conflict_blocks,
        winner.metrics.conflict_blocks,
        baseline.backfill_count,
        winner.metrics.backfill_count,
        baseline.locality_hits,
        winner.metrics.locality_hits,
    );
    let body = cut(&raw_body, cx.s.body_chars);
    let acceptance = format!(
        "Review replay evidence and either activate {proposed_version} via PUT /v1/scheduler/policy or drop with a verdict note"
    );
    let creator = server_reporter();
    cx.db.execute(
        "INSERT INTO tasks (id, board, number, title, body, acceptance, status, priority, auto, \
         created_by, version, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', 'p2', 0, ?7, 1, ?8, ?8)",
        params![
            id.as_str(),
            board.slug.as_str(),
            number,
            title,
            body,
            acceptance,
            creator.as_str(),
            now,
        ],
    )?;
    cx.emit(
        Some(&board.slug),
        actor,
        EventType::TaskCreated,
        Some(&id),
        serde_json::json!({
            "title": title,
            "priority": "p2",
            "autotune": true,
            "candidate": winner.name,
            "proposed_version": proposed_version,
            "score_delta_pct": winner.score_delta_pct,
        }),
    )?;
    Ok(id)
}

fn fmt_opt_secs(v: Option<f64>) -> String {
    match v {
        Some(s) => format!("{s:.1}s"),
        None => "-".to_owned(),
    }
}
