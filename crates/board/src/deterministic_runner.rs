//! Zero-LLM deterministic runner for `RunKind::Merge` and `RunKind::Housekeeping`
//! (`docs/SCHEDULER_DESIGN.md`, Section 4.3).
//!
//! Executes verified pull-request merges, `pr: none` / already-merged task completions,
//! gone-asker answer delivery, and away-owner task releases natively in Rust with zero LLM
//! tokens and zero agent process spawning.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{
    AgentId, AgentSource, AgentState, Board, DoneTask, ErrorCode, EvidenceKind, HousekeepingAction,
    NewEvidence, Note, NoteKind, Run, RunFilter, RunKind, RunOrigin, RunState, Task, TaskDetail,
    TaskId, TaskStatus, Timestamp, UpdateTask,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::client::{Client, error_code};
use crate::daemon::judges::{
    AFTER_MERGE_READY_NOTE, AdvanceOutcome, DispatchAskOutcome, JUDGE_NOTE, Merge, PR_HOLD_WAIT,
    PassedPr, ReviewVerdict, advance_pr, after_merge_checks, after_merge_loop_detected,
    after_merge_ready_info, collect_pass_checks, format_after_merge_ready_note,
    handle_dispatch_ask_answer, has_after_merge_ready_note_after, invalidates_review_pass,
    parked_in_review_at, passed_pr, passed_summary, reopen_passed_task, review_verdict,
};
use crate::daemon::{Worker, add_note, clip_note};
use crate::gh::{Checks, Gh, PullRequest};

/// Runner kind identifier advertised to `POST /v1/dispatch/claim` and device heartbeats.
pub const DETERMINISTIC_RUNNER_KIND: &str = "deterministic";

/// Prefix for the deduplicated finding note posted when a standing `review: pass` note names
/// an unclear or foreign PR URL.
const UNCLEAR_PR_NOTE_PREFIX: &str = "finding: merge skipped (unclear pr:";

/// Trail Rule `finding` note prefix posted when deterministic `deploy/upgrade.sh` after-merge
/// deployment fails and hands back to the LLM `AfterMerge` review judge (matches
/// `board-server/src/ops/runs.rs::DET_AFTER_MERGE_FALLBACK_PREFIX`).
pub const DET_AFTER_MERGE_FALLBACK_PREFIX: &str =
    "finding: deterministic after-merge deploy failed";

/// Maximum wall-clock timeout for a deterministic `deploy/upgrade.sh --yes <sha>` execution.
const DEPLOY_UPGRADE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Single source of truth for matching standard mechanical `deploy/upgrade.sh --yes`
/// `after merge:` checks, shared with `board-server`.
pub use board_server::is_standard_upgrade_deploy_check;

/// Returns `true` when `checks` is non-empty and EVERY listed `after merge:` check is a
/// standard mechanical `deploy/upgrade.sh` deploy eligible for `DeterministicRunner`.
pub fn is_standard_upgrade_deploy_checks(checks: &[String]) -> bool {
    !checks.is_empty() && checks.iter().all(|c| is_standard_upgrade_deploy_check(c))
}

/// Whether a dispatcher (`dispatch:*`) has already posted a deterministic after-merge
/// fallback finding note on `detail` after `pass_id`.
pub(crate) fn has_det_after_merge_fallback_note_after(detail: &TaskDetail, pass_id: i64) -> bool {
    detail.notes.iter().any(|n| {
        n.id > pass_id
            && n.agent.source() == AgentSource::Dispatch
            && n.text.starts_with(DET_AFTER_MERGE_FALLBACK_PREFIX)
    })
}

/// Output of one deterministic `deploy/upgrade.sh --yes <sha>` execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeRunOutput {
    pub exit_code: i32,
    pub output_tail: String,
}

/// Injectable executor for `deploy/upgrade.sh --yes <sha>` so deterministic after-merge
/// deploy behavior is cleanly unit-testable without running real SSH upgrades.
pub trait UpgradeDeployer: Send + Sync {
    fn run_upgrade<'a>(
        &'a self,
        project: &'a Path,
        sha: &'a str,
    ) -> BoxFuture<'a, Result<UpgradeRunOutput>>;
}

/// Production [`UpgradeDeployer`] invoking `<project>/deploy/upgrade.sh --yes <sha>`.
#[derive(Debug, Clone, Default)]
pub struct RealUpgradeDeployer;

impl UpgradeDeployer for RealUpgradeDeployer {
    fn run_upgrade<'a>(
        &'a self,
        project: &'a Path,
        sha: &'a str,
    ) -> BoxFuture<'a, Result<UpgradeRunOutput>> {
        Box::pin(async move {
            let script = project.join("deploy/upgrade.sh");
            if !script.is_file() {
                bail!("deploy script {} not found", script.display());
            }
            let child = tokio::process::Command::new(&script)
                .arg("--yes")
                .arg(sha)
                .current_dir(project)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("spawning {} --yes {sha}", script.display()))?;
            let Ok(out) =
                tokio::time::timeout(DEPLOY_UPGRADE_TIMEOUT, child.wait_with_output()).await
            else {
                bail!(
                    "deploy/upgrade.sh --yes {sha} timed out after {}s",
                    DEPLOY_UPGRADE_TIMEOUT.as_secs()
                );
            };
            let out = out.context("waiting for deploy/upgrade.sh")?;
            let exit_code = out.status.code().unwrap_or(-1);
            let combined = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let trimmed = combined.trim();
            let tail: String = trimmed
                .chars()
                .rev()
                .take(400)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            Ok(UpgradeRunOutput {
                exit_code,
                output_tail: tail,
            })
        })
    }
}

fn is_full_hex_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

async fn git_is_ancestor(project: &Path, ancestor: &str, descendant: &str) -> bool {
    tokio::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .current_dir(project)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

/// Outcome of one [`DeterministicRunner::execute_merge`] pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// PR was merged (or `pr: none` / already merged) and `task_done` succeeded.
    MergedAndDone {
        pr_url: Option<String>,
        merge_commit: Option<String>,
    },
    /// PR was merged (or `pr: none` / already merged), but the standing pass named
    /// `after merge:` acceptance checks; a Trail Rule handoff note was written so an
    /// after-merge review judge runs before finishing the task.
    AwaitingAfterMerge {
        pr_url: Option<String>,
        merge_commit: Option<String>,
    },
    /// Pass was voided (or already voided) and the task was returned to `open`.
    Reopened { reason: String },
    /// Required human review held past the limit: an `action` ask was created and the task
    /// moved to `blocked`.
    BlockedForReview { ask_id: i64, reason: String },
    /// PR is still waiting (checks pending, checks failed with finding noted, branch just
    /// updated, or worker still running). The run ends cleanly so the server immediately
    /// releases `repo:<repo>:merge` and re-offers on a future tick.
    Deferred { reason: String },
    /// Task is no longer in `review` or no longer has a standing pass.
    Noop { reason: String },
    /// Pass note names an ambiguous or foreign PR URL that cannot be merged automatically.
    Unclear { reason: String },
    /// Deterministic `deploy/upgrade.sh` after-merge deploy could not complete; a Trail Rule
    /// `finding` note was written so the server falls back to the LLM `AfterMerge` judge.
    FallbackToAfterMergeJudge { reason: String },
}

impl MergeOutcome {
    /// Whether this pass deferred waiting on GitHub checks or a running worker.
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred { .. })
    }

    /// Structured JSON outcome recorded on `PATCH /v1/runs/{id}`.
    pub fn to_outcome_json(&self) -> Value {
        match self {
            Self::MergedAndDone {
                pr_url,
                merge_commit,
            } => json!({
                "status": "merged_and_done",
                "pr_url": pr_url,
                "merge_commit": merge_commit,
            }),
            Self::AwaitingAfterMerge {
                pr_url,
                merge_commit,
            } => json!({
                "status": "awaiting_after_merge",
                "pr_url": pr_url,
                "merge_commit": merge_commit,
            }),
            Self::Reopened { reason } => json!({
                "status": "reopened",
                "reason": reason,
            }),
            Self::BlockedForReview { ask_id, reason } => json!({
                "status": "blocked_for_review",
                "ask_id": ask_id,
                "reason": reason,
            }),
            Self::Deferred { reason } => json!({
                "status": "deferred",
                "reason": reason,
            }),
            Self::Noop { reason } => json!({
                "status": "noop",
                "reason": reason,
            }),
            Self::Unclear { reason } => json!({
                "status": "unclear",
                "reason": reason,
            }),
            Self::FallbackToAfterMergeJudge { reason } => json!({
                "status": "fallback_after_merge",
                "reason": reason,
            }),
        }
    }
}

/// Outcome of one [`DeterministicRunner::execute_housekeeping`] pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HousekeepingOutcome {
    /// Posted the Trail Rule note and released `task_id` back to `open`.
    Opened {
        task_id: TaskId,
        action: &'static str,
    },
    /// Posted the Trail Rule note and returned `task_id` from `blocked` to `review`.
    ResumedReview {
        task_id: TaskId,
        action: &'static str,
    },
    /// Nothing remained to do (task already moved, ask missing, etc.).
    Noop { reason: String },
}

impl HousekeepingOutcome {
    /// Structured JSON outcome recorded on `PATCH /v1/runs/{id}`.
    pub fn to_outcome_json(&self) -> Value {
        match self {
            Self::Opened { task_id, action } => json!({
                "status": "opened",
                "task_id": task_id,
                "action": action,
            }),
            Self::ResumedReview { task_id, action } => json!({
                "status": "resumed_review",
                "task_id": task_id,
                "action": action,
            }),
            Self::Noop { reason } => json!({
                "status": "noop",
                "reason": reason,
            }),
        }
    }
}

/// Dispatcher-agnostic standing review evaluation for `DeterministicRunner`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StandingReview<'a> {
    Passed(&'a Note),
    Withdrawn,
    Waits,
}

/// Evaluates whether `detail` has a standing `review: pass` decision note written after the
/// task's latest move to `review` by either:
/// - a registered managed `review` [`Run`] for this task (on any device), or
/// - a legacy review judge noted by any device's `dispatch:<device>` agent.
///
/// Mirrors `board-server`'s `ops::runs::standing_review_pass` so cross-device handoffs
/// between worker, reviewer, and merger work seamlessly without trusting unverified notes.
pub(crate) fn standing_review<'a>(
    detail: &'a TaskDetail,
    review_runs: &[Run],
) -> StandingReview<'a> {
    let newest = detail.notes.iter().rev().find_map(|n| {
        (n.kind == NoteKind::Decision)
            .then(|| review_verdict(&n.text))
            .flatten()
            .map(|verdict| (n, verdict))
    });
    let Some((note, verdict)) = newest else {
        return StandingReview::Waits;
    };
    let parked_again = detail
        .history
        .iter()
        .any(|e| invalidates_review_pass(e) && e.at > note.at);
    if parked_again {
        return StandingReview::Waits;
    }
    match verdict {
        ReviewVerdict::Void => return StandingReview::Withdrawn,
        ReviewVerdict::Fail => return StandingReview::Waits,
        ReviewVerdict::Pass => {}
    }
    if detail.task.owner.as_ref() == Some(&note.agent) {
        return StandingReview::Waits;
    }
    let by_managed_run = review_runs.iter().any(|r| {
        r.origin == RunOrigin::Managed
            && r.kind == RunKind::Review
            && r.task_id.as_ref() == Some(&detail.task.id)
            && r.agent_id.as_ref() == Some(&note.agent)
            && r.state != RunState::Queued
            && (r.state != RunState::Cancelled || r.started_at.is_some())
    });
    let suffix = format!("(agent {})", note.agent);
    let by_legacy_judge = detail.notes.iter().any(|n| {
        n.agent.source() == board_proto::AgentSource::Dispatch
            && n.text.starts_with(JUDGE_NOTE)
            && n.text.ends_with(&suffix)
    });
    if by_managed_run || by_legacy_judge {
        StandingReview::Passed(note)
    } else {
        StandingReview::Waits
    }
}

/// Native Rust executor for zero-LLM `Merge` and `Housekeeping` runs.
#[derive(Debug, Clone, Default)]
pub struct DeterministicRunner;

impl DeterministicRunner {
    pub fn new() -> Self {
        Self
    }

    pub fn kind(&self) -> &'static str {
        DETERMINISTIC_RUNNER_KIND
    }

    /// Executes a claimed `RunKind::Merge` for `task_id` on `board`, reusing the verified
    /// PR check / `gh pr update-branch` / `gh pr merge --match-head-commit` / `task_done`
    /// logic. `client` must carry this device's `dispatch:<device>` agent id (authorized by
    /// the live `Merge` run without requiring a per-board coordinator lease).
    /// `resolve_project` resolves the local git checkout path for the fetched [`Task`]
    /// (honoring `task.repo` on multi-repo boards).
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_merge<F>(
        &self,
        board: &Board,
        task_id: &TaskId,
        agent: &AgentId,
        client: &Client,
        gh: &dyn Gh,
        resolve_project: F,
        merges: &mut BTreeMap<TaskId, Merge>,
        now: Timestamp,
    ) -> Result<MergeOutcome>
    where
        F: Fn(&Task) -> std::path::PathBuf,
    {
        self.execute_merge_with_deployer(
            board,
            task_id,
            agent,
            client,
            gh,
            &RealUpgradeDeployer,
            resolve_project,
            merges,
            now,
        )
        .await
    }

    /// Executes a claimed `after-merge-deploy` Deploy Train covering one or more merged tasks
    /// on `board`: deploys the newest descendant commit covering the batch ONCE via
    /// `deploy/upgrade.sh --yes <tip_sha>` and closes every covered task with that deploy check.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_deploy_train<F>(
        &self,
        board: &Board,
        task_ids: &[TaskId],
        agent: &AgentId,
        client: &Client,
        gh: &dyn Gh,
        resolve_project: F,
        merges: &mut BTreeMap<TaskId, Merge>,
        now: Timestamp,
    ) -> Result<MergeOutcome>
    where
        F: Fn(&Task) -> std::path::PathBuf,
    {
        self.execute_deploy_train_with_deployer(
            board,
            task_ids,
            agent,
            client,
            gh,
            &RealUpgradeDeployer,
            resolve_project,
            merges,
            now,
        )
        .await
    }

    /// Like [`Self::execute_deploy_train`], with an injectable [`UpgradeDeployer`] for testing.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_deploy_train_with_deployer<F>(
        &self,
        board: &Board,
        task_ids: &[TaskId],
        agent: &AgentId,
        client: &Client,
        gh: &dyn Gh,
        deployer: &dyn UpgradeDeployer,
        resolve_project: F,
        merges: &mut BTreeMap<TaskId, Merge>,
        now: Timestamp,
    ) -> Result<MergeOutcome>
    where
        F: Fn(&Task) -> std::path::PathBuf,
    {
        let Some(first_id) = task_ids.first() else {
            return Ok(MergeOutcome::Noop {
                reason: "empty deploy train".to_owned(),
            });
        };
        if task_ids.len() == 1 {
            return self
                .execute_merge_with_deployer(
                    board,
                    first_id,
                    agent,
                    client,
                    gh,
                    deployer,
                    resolve_project,
                    merges,
                    now,
                )
                .await;
        }

        struct TrainMember {
            detail: TaskDetail,
            pass: Note,
            pr_url: Option<String>,
            sha: String,
            pr_view: Option<PullRequest>,
            ready_at: Timestamp,
        }

        let mut members: Vec<TrainMember> = Vec::new();
        let mut project: Option<std::path::PathBuf> = None;
        for tid in task_ids {
            let detail = match client.task(tid).await {
                Ok(d) => d,
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => continue,
                Err(err) => return Err(err.context(format!("reading {tid} for deploy train"))),
            };
            if detail.task.status != TaskStatus::Review {
                continue;
            }
            let review_runs = match client
                .list_runs(&RunFilter {
                    task: Some(tid.clone()),
                    kind: Some(RunKind::Review),
                    origin: Some(RunOrigin::Managed),
                    limit: Some(50),
                    ..RunFilter::default()
                })
                .await
            {
                Ok(runs) => runs,
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => Vec::new(),
                Err(err) => return Err(err.context(format!("listing review runs for {tid}"))),
            };
            let pass = match standing_review(&detail, &review_runs) {
                StandingReview::Passed(pass) => pass.clone(),
                _ => continue,
            };
            let left_checks = after_merge_checks(&pass.text);
            if left_checks.is_empty()
                || !has_after_merge_ready_note_after(&detail, pass.id)
                || !is_standard_upgrade_deploy_checks(&left_checks)
                || has_det_after_merge_fallback_note_after(&detail, pass.id)
            {
                continue;
            }
            let proj = project
                .get_or_insert_with(|| resolve_project(&detail.task))
                .clone();
            let info = after_merge_ready_info(&detail, pass.id);
            let pr_url = info.as_ref().and_then(|i| i.pr_url.clone()).or_else(|| {
                match passed_pr(&detail, &pass, &board.repos) {
                    PassedPr::Url(url) => Some(url),
                    _ => None,
                }
            });
            let mut merge_commit = info.as_ref().and_then(|i| i.merge_commit.clone());
            let mut pr_view: Option<PullRequest> = None;
            if merge_commit.as_deref().is_none_or(|c| !is_full_hex_sha(c))
                && let Some(ref url) = pr_url
                && let Ok(pr) = gh.view(&proj, url).await
            {
                if let Some(ref mc) = pr.merge_commit
                    && is_full_hex_sha(mc)
                {
                    merge_commit = Some(mc.clone());
                }
                pr_view = Some(pr);
            }
            let Some(sha) = merge_commit.filter(|c| is_full_hex_sha(c)) else {
                let reason =
                    "no full 40-character merge commit SHA available for deploy/upgrade.sh"
                        .to_owned();
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                add_note(client, &detail.task.id, agent, NoteKind::Finding, text).await?;
                continue;
            };
            let ready_at = detail
                .notes
                .iter()
                .rev()
                .find(|n| {
                    n.id > pass.id
                        && n.agent.source() == AgentSource::Dispatch
                        && n.text.starts_with(AFTER_MERGE_READY_NOTE)
                })
                .map(|n| n.at)
                .unwrap_or(pass.at);
            members.push(TrainMember {
                detail,
                pass,
                pr_url,
                sha,
                pr_view,
                ready_at,
            });
        }

        if members.is_empty() {
            return Ok(MergeOutcome::Noop {
                reason: "no deploy-eligible tasks remained in deploy train".to_owned(),
            });
        }
        let proj = project.expect("members non-empty implies project set");

        // Best-effort fetch so `git merge-base --is-ancestor` sees newly merged commits.
        let is_git_repo = proj.join(".git").exists();
        if is_git_repo {
            let _ = tokio::process::Command::new("git")
                .args(["fetch", "--quiet", "origin"])
                .current_dir(&proj)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }

        // Choose the newest descendant commit in the train as `tip_sha`.
        members.sort_by(|a, b| {
            a.ready_at
                .cmp(&b.ready_at)
                .then_with(|| a.detail.task.id.cmp(&b.detail.task.id))
        });
        let mut tip_idx = members.len() - 1;
        if is_git_repo {
            for idx in 0..members.len() {
                if idx == tip_idx {
                    continue;
                }
                if git_is_ancestor(&proj, &members[tip_idx].sha, &members[idx].sha).await {
                    tip_idx = idx;
                }
            }
        }
        let tip_sha = members[tip_idx].sha.clone();
        let tip_pr_url = members[tip_idx].pr_url.clone();

        // Keep only tasks whose merge commit is covered by `tip_sha` (or all members when
        // running against a non-git test directory with a fake deployer).
        let mut covered: Vec<TrainMember> = Vec::with_capacity(members.len());
        for m in members {
            if !is_git_repo || m.sha == tip_sha || git_is_ancestor(&proj, &m.sha, &tip_sha).await {
                covered.push(m);
            }
        }
        let oldest_ready_at = covered.iter().map(|m| m.ready_at).min().unwrap_or(now);

        // Wait for `main` CI `image` job on `tip_sha` before deploying.
        match gh
            .commit_check_status(&proj, tip_pr_url.as_deref(), &tip_sha, "image")
            .await
        {
            Ok(Checks::Passed) => {}
            Ok(Checks::Pending) => {
                if now - oldest_ready_at >= PR_HOLD_WAIT {
                    let reason = format!(
                        "main CI `image` check on {tip_sha} stayed pending past {}m",
                        PR_HOLD_WAIT.num_minutes()
                    );
                    let text = clip_note(format!(
                        "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                    ));
                    for m in &covered {
                        add_note(
                            client,
                            &m.detail.task.id,
                            agent,
                            NoteKind::Finding,
                            text.clone(),
                        )
                        .await?;
                    }
                    return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
                }
                return Ok(MergeOutcome::Deferred {
                    reason: format!(
                        "waiting for main CI `image` job on deploy train tip {tip_sha}"
                    ),
                });
            }
            Ok(Checks::Failed(names)) => {
                let reason = format!(
                    "main CI `image` check failed on {tip_sha} ({})",
                    names.join(", ")
                );
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                for m in &covered {
                    add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Finding,
                        text.clone(),
                    )
                    .await?;
                }
                return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
            }
            Err(err) => {
                let reason =
                    format!("reading main CI `image` status for {tip_sha} failed: {err:#}");
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                for m in &covered {
                    add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Finding,
                        text.clone(),
                    )
                    .await?;
                }
                return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
            }
        }

        // Execute `deploy/upgrade.sh --yes <tip_sha>` ONCE for the whole train.
        let train_ids: Vec<String> = covered
            .iter()
            .map(|m| m.detail.task.id.to_string())
            .collect();
        match deployer.run_upgrade(&proj, &tip_sha).await {
            Ok(out) if out.exit_code == 0 => {
                let mut closed = 0usize;
                let mut close_errors: Vec<String> = Vec::new();
                for m in &covered {
                    let fresh_detail = match client.task(&m.detail.task.id).await {
                        Ok(d) => d,
                        Err(_) => m.detail.clone(),
                    };
                    if fresh_detail.task.status == TaskStatus::Done {
                        closed += 1;
                        merges.remove(&m.detail.task.id);
                        continue;
                    }
                    let mut extra = vec![NewEvidence {
                        kind: EvidenceKind::Check,
                        value: format!("deploy/upgrade.sh --yes {tip_sha}"),
                        exit_code: Some(0),
                    }];
                    if m.pr_view.is_none() {
                        if let Some(ref url) = m.pr_url {
                            extra.push(NewEvidence {
                                kind: EvidenceKind::Pr,
                                value: url.clone(),
                                exit_code: None,
                            });
                        }
                        extra.push(NewEvidence {
                            kind: EvidenceKind::Commit,
                            value: m.sha.clone(),
                            exit_code: None,
                        });
                    }
                    if let Err(err) = self
                        .finish_task_with_extra(
                            agent,
                            client,
                            &fresh_detail,
                            &m.pass,
                            m.pr_view.as_ref(),
                            extra,
                        )
                        .await
                    {
                        tracing::warn!(
                            "{}: deploy train {tip_sha} succeeded, but closing task failed: {err:#}",
                            m.detail.task.id
                        );
                        close_errors.push(format!("{}: {err:#}", m.detail.task.id));
                        continue;
                    }
                    closed += 1;
                    let pr_line = match &m.pr_url {
                        Some(url) => format!("pr: {url}"),
                        None => "pr: none".to_owned(),
                    };
                    let pass_text = clip_note(format!(
                        "review: pass. Deployed in {}-task deploy train at {tip_sha} (commit {}).\n\
                         check: deploy/upgrade.sh --yes {tip_sha} => 0\n\
                         {pr_line}",
                        covered.len(),
                        m.sha
                    ));
                    if let Err(err) = add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Decision,
                        pass_text,
                    )
                    .await
                    {
                        tracing::warn!(
                            "{}: deployed in train {tip_sha}, but recording trailing pass note failed: {err:#}",
                            m.detail.task.id
                        );
                    }
                    merges.remove(&m.detail.task.id);
                }
                if closed == 0 && !close_errors.is_empty() {
                    bail!(
                        "deploy train at {tip_sha} succeeded, but closing all covered tasks failed: {}",
                        close_errors.join("; ")
                    );
                }
                tracing::info!(
                    "deploy train finished {closed}/{} task(s) [{}] at {tip_sha}",
                    covered.len(),
                    train_ids.join(", ")
                );
                Ok(MergeOutcome::MergedAndDone {
                    pr_url: tip_pr_url,
                    merge_commit: Some(tip_sha),
                })
            }
            Ok(out) => {
                let tail = if out.output_tail.is_empty() {
                    "no output".to_owned()
                } else {
                    out.output_tail
                };
                let reason = format!(
                    "deploy/upgrade.sh --yes {tip_sha} (train [{}]) exited {}: {tail}",
                    train_ids.join(", "),
                    out.exit_code
                );
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                for m in &covered {
                    add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Finding,
                        text.clone(),
                    )
                    .await?;
                }
                Ok(MergeOutcome::FallbackToAfterMergeJudge { reason })
            }
            Err(err) => {
                let reason = format!(
                    "deploy/upgrade.sh --yes {tip_sha} (train [{}]) error: {err:#}",
                    train_ids.join(", ")
                );
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                for m in &covered {
                    add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Finding,
                        text.clone(),
                    )
                    .await?;
                }
                Ok(MergeOutcome::FallbackToAfterMergeJudge { reason })
            }
        }
    }

    /// Like [`Self::execute_merge`], with an injectable [`UpgradeDeployer`] for testing
    /// zero-LLM deterministic `deploy/upgrade.sh --yes <sha>` after-merge deployments.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_merge_with_deployer<F>(
        &self,
        board: &Board,
        task_id: &TaskId,
        agent: &AgentId,
        client: &Client,
        gh: &dyn Gh,
        deployer: &dyn UpgradeDeployer,
        resolve_project: F,
        merges: &mut BTreeMap<TaskId, Merge>,
        now: Timestamp,
    ) -> Result<MergeOutcome>
    where
        F: Fn(&Task) -> std::path::PathBuf,
    {
        let detail = match client.task(task_id).await {
            Ok(detail) => detail,
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                merges.remove(task_id);
                return Ok(MergeOutcome::Noop {
                    reason: format!("{task_id} no longer exists"),
                });
            }
            Err(err) => return Err(err.context(format!("reading {task_id} for merge"))),
        };
        let project = resolve_project(&detail.task);
        if detail.task.status != TaskStatus::Review {
            merges.remove(task_id);
            return Ok(MergeOutcome::Noop {
                reason: format!("{task_id} is no longer in review"),
            });
        }
        let review_runs = match client
            .list_runs(&RunFilter {
                task: Some(task_id.clone()),
                kind: Some(RunKind::Review),
                origin: Some(RunOrigin::Managed),
                limit: Some(50),
                ..RunFilter::default()
            })
            .await
        {
            Ok(runs) => runs,
            Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => Vec::new(),
            Err(err) => {
                return Err(err.context(format!("listing review runs for {task_id}")));
            }
        };
        let pass = match standing_review(&detail, &review_runs) {
            StandingReview::Passed(pass) => pass,
            StandingReview::Withdrawn => {
                client
                    .task_action(task_id, "release")
                    .await
                    .with_context(|| format!("setting {task_id} open after review: void"))?;
                merges.remove(task_id);
                return Ok(MergeOutcome::Reopened {
                    reason: "review pass was previously voided".to_owned(),
                });
            }
            StandingReview::Waits => {
                return Ok(MergeOutcome::Noop {
                    reason: format!("{task_id} has no standing review: pass"),
                });
            }
        };

        let left_checks = after_merge_checks(&pass.text);
        if !left_checks.is_empty() && has_after_merge_ready_note_after(&detail, pass.id) {
            if is_standard_upgrade_deploy_checks(&left_checks)
                && !has_det_after_merge_fallback_note_after(&detail, pass.id)
            {
                return self
                    .execute_deterministic_after_merge_deploy(
                        board, &detail, pass, agent, client, gh, deployer, &project, merges, now,
                    )
                    .await;
            }
            return Ok(MergeOutcome::Deferred {
                reason: "already merged; waiting for after-merge review judge".to_owned(),
            });
        }

        let task = &detail.task;
        // Wait while the worker that owns the task is still actively running.
        if let Some(owner) = task.owner.as_ref().filter(|owner| *owner != agent) {
            let running = match client.agent_info(owner).await {
                Ok(info) => info.state == AgentState::Running,
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => false,
                Err(err) => return Err(err.context(format!("reading owner {owner}"))),
            };
            if running {
                return Ok(MergeOutcome::Deferred {
                    reason: format!("task owner {owner} is still running"),
                });
            }
        }

        let passed = passed_pr(&detail, pass, &board.repos);
        // Reject loop-capped after-merge passes BEFORE calling `advance_pr` so a capped
        // follow-up PR is never merged before being voided and reopened.
        if !left_checks.is_empty() {
            let planned_pr = match &passed {
                PassedPr::None => Some(None),
                PassedPr::Url(url) => Some(Some(url.as_str())),
                PassedPr::Unclear(_) => None,
            };
            if let Some(pr_opt) = planned_pr
                && after_merge_loop_detected(&detail, pass.id, pr_opt)
            {
                let text = clip_note(format!(
                    "finding: the after-merge review pass still left post-merge checks unrun (`after merge:` lines remained after the merge): {}",
                    left_checks.join("; ")
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                let why = "its after-merge review pass left post-merge checks unrun";
                reopen_passed_task(agent, client, task, why).await?;
                merges.remove(&task.id);
                return Ok(MergeOutcome::Reopened {
                    reason: why.to_owned(),
                });
            }
        }
        let merged = match passed {
            PassedPr::None => None,
            PassedPr::Unclear(why) => {
                let already_noted = detail.notes.iter().any(|n| {
                    n.kind == NoteKind::Finding
                        && n.id > pass.id
                        && n.at >= pass.at
                        && n.text.starts_with(UNCLEAR_PR_NOTE_PREFIX)
                });
                if !already_noted {
                    let text = clip_note(format!(
                        "{UNCLEAR_PR_NOTE_PREFIX} {why}). Post a new `review: pass` note naming \
                         the pull request in a `pr:` line (or `pr: none` if no PR applies)."
                    ));
                    add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                }
                tracing::warn!(
                    "{}: passed, but not merged or finished: {why}. Name its pull request in a `pr:` line of a new pass note",
                    task.id
                );
                return Ok(MergeOutcome::Unclear { reason: why });
            }
            PassedPr::Url(url) => {
                let parked_at = parked_in_review_at(&detail);
                let mut merge = match merges.get(&task.id) {
                    Some(existing) => {
                        let mut m = existing.clone();
                        m.reset_if_before(parked_at);
                        m
                    }
                    None => Merge::new(now),
                };
                merge.read_at = now;
                merges.insert(task.id.clone(), merge.clone());

                let pr = gh
                    .view(&project, &url)
                    .await
                    .with_context(|| format!("reading {url}"))?;
                match advance_pr(
                    agent, client, gh, &project, &detail, &url, pr, merges, &mut merge, now,
                )
                .await?
                {
                    AdvanceOutcome::Merged(pr) => Some(pr),
                    AdvanceOutcome::Reopened(reason) => {
                        return Ok(MergeOutcome::Reopened { reason });
                    }
                    AdvanceOutcome::BlockedForReview { ask_id, reason } => {
                        return Ok(MergeOutcome::BlockedForReview { ask_id, reason });
                    }
                    AdvanceOutcome::Deferred(reason) => {
                        return Ok(MergeOutcome::Deferred { reason });
                    }
                }
            }
        };

        if !left_checks.is_empty() {
            let current_pr = merged.as_ref().map(|pr| pr.url.as_str());
            if after_merge_loop_detected(&detail, pass.id, current_pr) {
                let text = clip_note(format!(
                    "finding: the after-merge review pass still left post-merge checks unrun (`after merge:` lines remained after the merge): {}",
                    left_checks.join("; ")
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                let why = "its after-merge review pass left post-merge checks unrun";
                reopen_passed_task(agent, client, task, why).await?;
                merges.remove(&task.id);
                return Ok(MergeOutcome::Reopened {
                    reason: why.to_owned(),
                });
            }
            let note_text = format_after_merge_ready_note(pass.id, merged.as_ref(), &left_checks);
            add_note(client, &task.id, agent, NoteKind::Note, note_text).await?;
            merges.remove(&task.id);
            let pr_url = merged.as_ref().map(|pr| pr.url.clone());
            let merge_commit = merged.as_ref().and_then(|pr| pr.merge_commit.clone());
            tracing::info!(
                "{}: merged (or no PR); waiting for after-merge review judge for {} left check(s)",
                task.id,
                left_checks.len()
            );
            return Ok(MergeOutcome::AwaitingAfterMerge {
                pr_url,
                merge_commit,
            });
        }

        self.finish_task(agent, client, &detail, pass, merged.as_ref())
            .await?;
        merges.remove(&task.id);
        match &merged {
            Some(pr) => {
                tracing::info!(
                    "{}: finished after review pass via deterministic runner; {} is merged",
                    task.id,
                    pr.url
                );
                Ok(MergeOutcome::MergedAndDone {
                    pr_url: Some(pr.url.clone()),
                    merge_commit: pr.merge_commit.clone(),
                })
            }
            None => {
                tracing::info!(
                    "{}: finished after review pass via deterministic runner; no pull request",
                    task.id
                );
                Ok(MergeOutcome::MergedAndDone {
                    pr_url: None,
                    merge_commit: None,
                })
            }
        }
    }

    /// Executes a claimed `RunKind::Housekeeping` action natively in Rust.
    pub async fn execute_housekeeping(
        &self,
        action: &HousekeepingAction,
        agent: &AgentId,
        client: &Client,
        workers: &BTreeMap<TaskId, Worker>,
    ) -> Result<HousekeepingOutcome> {
        match action {
            HousekeepingAction::AnswerGoneAsker {
                ask_id,
                task_id,
                asker,
            } => {
                let found = match client.ask(*ask_id).await {
                    Ok(ask) => ask,
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                        return Ok(HousekeepingOutcome::Noop {
                            reason: format!("ask #{ask_id} no longer exists"),
                        });
                    }
                    Err(err) => return Err(err.context(format!("reading ask #{ask_id}"))),
                };
                let (Some(ask_task), Some(answer)) = (found.task, found.answer) else {
                    return Ok(HousekeepingOutcome::Noop {
                        reason: format!("ask #{ask_id} has no task or answer"),
                    });
                };
                let target_task = ask_task;
                if target_task != *task_id {
                    tracing::debug!(
                        "housekeeping ask #{ask_id} belongs to {target_task} (claimed for {task_id})"
                    );
                }
                let asker_id = asker.clone().unwrap_or(found.agent);
                let detail = match client.task(&target_task).await {
                    Ok(detail) => detail,
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                        return Ok(HousekeepingOutcome::Noop {
                            reason: format!("{target_task} no longer exists"),
                        });
                    }
                    Err(err) => return Err(err.context(format!("reading {target_task}"))),
                };
                let answered_at = found.answered_at.unwrap_or(found.created_at);
                let marker = format!("ask #{ask_id}");
                let mut already_noted = detail.notes.iter().any(|n| {
                    n.kind == NoteKind::Decision && n.at >= answered_at && n.text.contains(&marker)
                });
                if asker_id.source() == AgentSource::Dispatch {
                    return match handle_dispatch_ask_answer(
                        client,
                        agent,
                        &detail.task,
                        *ask_id,
                        &asker_id,
                        &answer,
                        &mut already_noted,
                    )
                    .await?
                    {
                        DispatchAskOutcome::Reopened => Ok(HousekeepingOutcome::Opened {
                            task_id: target_task,
                            action: "answer_dispatch_ask",
                        }),
                        DispatchAskOutcome::ResumedReview => {
                            Ok(HousekeepingOutcome::ResumedReview {
                                task_id: target_task,
                                action: "answer_dispatch_ask",
                            })
                        }
                        DispatchAskOutcome::Noop(reason) => {
                            Ok(HousekeepingOutcome::Noop { reason })
                        }
                    };
                }
                if !already_noted {
                    let text = clip_note(format!(
                        "decision: the user answered ask #{ask_id} from {asker_id}, which cannot be woken: {answer}"
                    ));
                    add_note(client, &target_task, agent, NoteKind::Decision, text).await?;
                }
                let held = matches!(
                    detail.task.status,
                    TaskStatus::Doing | TaskStatus::Blocked | TaskStatus::Review
                ) && detail
                    .task
                    .owner
                    .as_ref()
                    .is_none_or(|owner| owner == &asker_id);
                if !held {
                    return Ok(HousekeepingOutcome::Noop {
                        reason: format!("{target_task} is no longer held by {asker_id}"),
                    });
                }
                client
                    .task_action(&target_task, "release")
                    .await
                    .with_context(|| format!("setting {target_task} open after ask #{ask_id}"))?;
                tracing::info!(
                    "{target_task}: ask #{ask_id} answered for gone {asker_id}; set open via deterministic runner"
                );
                Ok(HousekeepingOutcome::Opened {
                    task_id: target_task,
                    action: "answer_gone_asker",
                })
            }
            HousekeepingAction::ReleaseAwayOwner {
                task_id,
                owner,
                away_since,
            } => {
                let detail = match client.task(task_id).await {
                    Ok(detail) => detail,
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => {
                        return Ok(HousekeepingOutcome::Noop {
                            reason: format!("{task_id} no longer exists"),
                        });
                    }
                    Err(err) => return Err(err.context(format!("reading {task_id}"))),
                };
                if detail.task.status != TaskStatus::Doing
                    || detail.task.owner.as_ref() != Some(owner)
                {
                    return Ok(HousekeepingOutcome::Noop {
                        reason: format!("{task_id} is no longer in doing under {owner}"),
                    });
                }
                let info = match client.agent_info(owner).await {
                    Ok(info) => Some(info),
                    Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => None,
                    Err(err) => return Err(err.context(format!("reading away owner {owner}"))),
                };
                let thread = info
                    .as_ref()
                    .and_then(|i| i.native_ids.get("t3_link"))
                    .map_or_else(|| owner.to_string(), |link| format!("{owner} ({link})"));
                let branch = workers
                    .get(task_id)
                    .and_then(|w| w.branch.clone())
                    .or_else(|| info.and_then(|i| i.branch))
                    .map_or_else(|| "none recorded".to_owned(), |b| format!("`{b}`"));
                let since_at = away_since.unwrap_or(detail.task.updated_at);
                let text = clip_note(format!(
                    "note: the owner was away from this task since {} and still was after the \
                     server's repeat notice, or its thread is gone, so the dispatcher set it \
                     open for a new worker. Old thread: {thread}. Branch: {branch}.",
                    since_at.format("%Y-%m-%d %H:%MZ")
                ));
                add_note(client, task_id, agent, NoteKind::Note, text).await?;
                client
                    .task_action(task_id, "release")
                    .await
                    .with_context(|| format!("setting {task_id} open from away {owner}"))?;
                tracing::info!(
                    "{task_id}: {owner} away since {since_at}; set open via deterministic runner"
                );
                Ok(HousekeepingOutcome::Opened {
                    task_id: task_id.clone(),
                    action: "release_away_owner",
                })
            }
            HousekeepingAction::Unknown => {
                bail!("unsupported housekeeping action from a newer server; upgrade board")
            }
        }
    }

    /// Executes a standard mechanical `deploy/upgrade.sh --yes <sha>` after-merge check at
    /// 0 LLM tokens:
    /// 1. Resolves the merged commit SHA (from the `AFTER_MERGE_READY_NOTE` or `gh.view`),
    /// 2. Checks that the `main` CI `image` job succeeded for `<sha>` (deferring with server
    ///    backoff while pending),
    /// 3. Runs `deploy/upgrade.sh --yes <sha>` via `deployer`,
    /// 4. On exit 0, posts a `review: pass` decision note and closes the task with evidence;
    ///    on any failure or timeout, posts a Trail Rule fallback `finding` note so the server
    ///    falls back cleanly to the LLM `AfterMerge` review judge.
    #[allow(clippy::too_many_arguments)]
    async fn execute_deterministic_after_merge_deploy(
        &self,
        board: &Board,
        detail: &TaskDetail,
        pass: &Note,
        agent: &AgentId,
        client: &Client,
        gh: &dyn Gh,
        deployer: &dyn UpgradeDeployer,
        project: &Path,
        merges: &mut BTreeMap<TaskId, Merge>,
        now: Timestamp,
    ) -> Result<MergeOutcome> {
        let task = &detail.task;
        let info = after_merge_ready_info(detail, pass.id);
        let pr_url = info.as_ref().and_then(|i| i.pr_url.clone()).or_else(|| {
            match passed_pr(detail, pass, &board.repos) {
                PassedPr::Url(url) => Some(url),
                _ => None,
            }
        });
        let mut merge_commit = info.as_ref().and_then(|i| i.merge_commit.clone());
        let mut pr_view: Option<PullRequest> = None;
        if merge_commit.as_deref().is_none_or(|c| !is_full_hex_sha(c))
            && let Some(ref url) = pr_url
            && let Ok(pr) = gh.view(project, url).await
        {
            if let Some(ref mc) = pr.merge_commit
                && is_full_hex_sha(mc)
            {
                merge_commit = Some(mc.clone());
            }
            pr_view = Some(pr);
        }
        let Some(sha) = merge_commit.filter(|c| is_full_hex_sha(c)) else {
            let reason =
                "no full 40-character merge commit SHA available for deploy/upgrade.sh".to_owned();
            let text = clip_note(format!(
                "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
            ));
            add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
            return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
        };

        // Wait for `main` CI `image` job on `sha` before invoking `deploy/upgrade.sh --yes`.
        match gh
            .commit_check_status(project, pr_url.as_deref(), &sha, "image")
            .await
        {
            Ok(Checks::Passed) => {}
            Ok(Checks::Pending) => {
                let ready_at = detail
                    .notes
                    .iter()
                    .rev()
                    .find(|n| {
                        n.id > pass.id
                            && n.agent.source() == AgentSource::Dispatch
                            && n.text.starts_with(AFTER_MERGE_READY_NOTE)
                    })
                    .map(|n| n.at)
                    .unwrap_or(pass.at);
                if now - ready_at >= PR_HOLD_WAIT {
                    let reason = format!(
                        "main CI `image` check on {sha} stayed pending past {}m",
                        PR_HOLD_WAIT.num_minutes()
                    );
                    let text = clip_note(format!(
                        "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                    ));
                    add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                    return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
                }
                return Ok(MergeOutcome::Deferred {
                    reason: format!("waiting for main CI `image` job on {sha}"),
                });
            }
            Ok(Checks::Failed(names)) => {
                let reason = format!(
                    "main CI `image` check failed on {sha} ({})",
                    names.join(", ")
                );
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
            }
            Err(err) => {
                let reason = format!("reading main CI `image` status for {sha} failed: {err:#}");
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
            }
        }

        // Execute `deploy/upgrade.sh --yes <sha>`.
        match deployer.run_upgrade(project, &sha).await {
            Ok(out) if out.exit_code == 0 => {
                let mut extra = vec![NewEvidence {
                    kind: EvidenceKind::Check,
                    value: format!("deploy/upgrade.sh --yes {sha}"),
                    exit_code: Some(0),
                }];
                if pr_view.is_none() {
                    if let Some(ref url) = pr_url {
                        extra.push(NewEvidence {
                            kind: EvidenceKind::Pr,
                            value: url.clone(),
                            exit_code: None,
                        });
                    }
                    extra.push(NewEvidence {
                        kind: EvidenceKind::Commit,
                        value: sha.clone(),
                        exit_code: None,
                    });
                }
                // Close the task FIRST so a transient post-restart 502/503 on `task_done`
                // never leaves a `dispatch:*` `review: pass.` decision note that would shadow
                // the original registered reviewer's standing pass.
                self.finish_task_with_extra(agent, client, detail, pass, pr_view.as_ref(), extra)
                    .await?;
                let pr_line = match &pr_url {
                    Some(url) => format!("pr: {url}"),
                    None => "pr: none".to_owned(),
                };
                let pass_text = clip_note(format!(
                    "review: pass. Deployed merged commit {sha} via deterministic after-merge runner.\n\
                     check: deploy/upgrade.sh --yes {sha} => 0\n\
                     {pr_line}"
                ));
                if let Err(err) =
                    add_note(client, &task.id, agent, NoteKind::Decision, pass_text).await
                {
                    tracing::warn!(
                        "{}: deployed and finished, but recording trailing deploy pass note failed: {err:#}",
                        task.id
                    );
                }
                merges.remove(&task.id);
                tracing::info!(
                    "{}: deployed {sha} and finished via deterministic after-merge runner",
                    task.id
                );
                Ok(MergeOutcome::MergedAndDone {
                    pr_url,
                    merge_commit: Some(sha),
                })
            }
            Ok(out) => {
                let tail = if out.output_tail.is_empty() {
                    "no output".to_owned()
                } else {
                    out.output_tail
                };
                let reason = format!(
                    "deploy/upgrade.sh --yes {sha} exited {}: {tail}",
                    out.exit_code
                );
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                Ok(MergeOutcome::FallbackToAfterMergeJudge { reason })
            }
            Err(err) => {
                let reason = format!("deploy/upgrade.sh --yes {sha} error: {err:#}");
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                Ok(MergeOutcome::FallbackToAfterMergeJudge { reason })
            }
        }
    }

    async fn finish_task(
        &self,
        agent: &AgentId,
        client: &Client,
        detail: &TaskDetail,
        pass: &Note,
        merged: Option<&PullRequest>,
    ) -> Result<()> {
        self.finish_task_with_extra(agent, client, detail, pass, merged, Vec::new())
            .await
    }

    async fn finish_task_with_extra(
        &self,
        agent: &AgentId,
        client: &Client,
        detail: &TaskDetail,
        pass: &Note,
        merged: Option<&PullRequest>,
        extra_evidence: Vec<NewEvidence>,
    ) -> Result<()> {
        let task = &detail.task;
        let mut evidence = Vec::new();
        if let Some(pr) = merged {
            evidence.push(NewEvidence {
                kind: EvidenceKind::Pr,
                value: pr.url.clone(),
                exit_code: None,
            });
            if let Some(commit) = &pr.merge_commit {
                evidence.push(NewEvidence {
                    kind: EvidenceKind::Commit,
                    value: commit.clone(),
                    exit_code: None,
                });
            }
        }
        evidence.extend(collect_pass_checks(detail, pass));
        for item in extra_evidence {
            if !evidence.iter().any(|e| {
                e.kind == item.kind && e.value == item.value && e.exit_code == item.exit_code
            }) {
                evidence.push(item);
            }
        }
        if evidence.is_empty() {
            evidence.push(NewEvidence {
                kind: EvidenceKind::Artifact,
                value: format!("the review pass in note #{} of {}", pass.id, task.id),
                exit_code: None,
            });
        }
        let mut version = task.version;
        if task.owner.as_ref() != Some(agent) {
            let handed = client
                .update_task(
                    &task.id,
                    &UpdateTask {
                        agent: Some(agent.clone().into()),
                        version,
                        status: None,
                        title: None,
                        body: None,
                        acceptance: None,
                        priority: None,
                        blockers: None,
                        blocked_reason: None,
                        auto: None,
                        not_before: None,
                        rank: None,
                        owner: Some(agent.clone()),
                        reason: None,
                        repo: None,
                        weight: None,
                        resource_units: None,
                        requires: None,
                        tolerations: None,
                        conflict_keys: None,
                        affinity: None,
                        preferred_runner: None,
                        size: None,
                        cooled: None,
                        before: None,
                        after: None,
                    },
                )
                .await
                .with_context(|| format!("handing {} to the deterministic runner", task.id))?;
            version = handed.version;
        }
        client
            .done(
                &task.id,
                &DoneTask {
                    agent: agent.clone(),
                    version,
                    evidence,
                    summary: Some(passed_summary(&pass.text)),
                    take: false,
                },
            )
            .await
            .with_context(|| format!("finishing {}", task.id))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: i64, agent: &str, kind: NoteKind, text: &str, at: &str) -> Note {
        Note {
            id,
            task: "factory-7".parse().unwrap(),
            agent: agent.parse().unwrap(),
            kind,
            text: text.to_owned(),
            at: at.parse().unwrap(),
        }
    }

    fn detail(notes: Vec<Note>, history: Value) -> TaskDetail {
        serde_json::from_value(json!({
            "task": {
                "id": "factory-7", "board": "factory", "title": "Fix it", "body": "",
                "acceptance": "", "status": "review", "priority": "p2",
                "owner": "t3:worker", "owner_stale": false, "blockers": [], "ready": false,
                "auto": true, "parent": null, "blocked_reason": null,
                "created_by": "t3:user", "version": 4,
                "created_at": "2026-11-08T10:00:00Z", "updated_at": "2026-11-08T11:00:00Z",
            },
            "notes": notes, "evidence": [], "blocks": [], "children": [],
            "history": history,
        }))
        .unwrap()
    }

    fn review_run(agent: &str, state: RunState) -> Run {
        let ts: Timestamp = "2026-11-08T11:30:00Z".parse().unwrap();
        Run {
            id: "run-rev-1".parse().unwrap(),
            task_id: Some("factory-7".parse().unwrap()),
            board: Some("factory".parse().unwrap()),
            kind: RunKind::Review,
            origin: RunOrigin::Managed,
            device: "mac-pro".parse().unwrap(),
            runner: "acp".to_owned(),
            agent_id: Some(agent.parse().unwrap()),
            state,
            lease_expires_at: None,
            worktree_branch: None,
            cwd: None,
            title: None,
            attach_hint: None,
            cost_units: 2,
            policy_version: None,
            outcome: None,
            transcript_uri: None,
            transcript_sha256: None,
            conflict_locks: Vec::new(),
            created_at: ts,
            started_at: Some(ts),
            ended_at: Some(ts),
            updated_at: ts,
        }
    }

    #[test]
    fn standing_review_accepts_cross_device_managed_review_run_and_rejects_unverified_notes() {
        let pass = note(
            2,
            "antigravity:rev-1",
            NoteKind::Decision,
            "review: pass. Clean diff.\npr: none",
            "2026-11-08T12:00:00Z",
        );
        let parked_before = json!([{
            "seq": 9, "at": "2026-11-08T11:00:00Z", "board": "factory", "actor": "t3:worker",
            "type": "task_updated", "task": "factory-7",
            "payload": {"from": "doing", "to": "review"},
        }]);
        let d = detail(vec![pass.clone()], parked_before.clone());

        // Without a registered review run or legacy dispatch note, pass is rejected.
        assert_eq!(standing_review(&d, &[]), StandingReview::Waits);

        // With a managed review Run from another device (mac-pro), pass stands.
        let run = review_run("antigravity:rev-1", RunState::Succeeded);
        assert!(matches!(
            standing_review(&d, std::slice::from_ref(&run)),
            StandingReview::Passed(n) if n.id == 2
        ));

        // A queued run never authorizes a pass.
        let queued = review_run("antigravity:rev-1", RunState::Queued);
        assert_eq!(standing_review(&d, &[queued]), StandingReview::Waits);

        // A legacy dispatch:* note from another device also authorizes the pass.
        let legacy_start = note(
            1,
            "dispatch:mac-other",
            NoteKind::Note,
            &format!("{JUDGE_NOTE} thread j (agent antigravity:rev-1)"),
            "2026-11-08T11:30:00Z",
        );
        let d_legacy = detail(vec![legacy_start, pass.clone()], parked_before);
        assert!(matches!(
            standing_review(&d_legacy, &[]),
            StandingReview::Passed(n) if n.id == 2
        ));

        // Task owner cannot self-pass even if a run listed them.
        let self_pass = note(
            3,
            "t3:worker",
            NoteKind::Decision,
            "review: pass",
            "2026-11-08T12:00:00Z",
        );
        let self_run = review_run("t3:worker", RunState::Succeeded);
        let d_self = detail(vec![self_pass], json!([]));
        assert_eq!(standing_review(&d_self, &[self_run]), StandingReview::Waits);

        // A later review: void marks it Withdrawn.
        let void = note(
            4,
            "dispatch:mac-air",
            NoteKind::Decision,
            "review: void. Conflict.",
            "2026-11-08T12:30:00Z",
        );
        let d_void = detail(vec![pass, void], json!([]));
        assert_eq!(standing_review(&d_void, &[run]), StandingReview::Withdrawn);
    }

    #[test]
    fn standard_upgrade_deploy_check_matcher_distinguishes_mechanical_from_custom_checks() {
        assert!(is_standard_upgrade_deploy_checks(&[
            "deploy/upgrade.sh --yes".to_owned()
        ]));
        assert!(is_standard_upgrade_deploy_checks(&[
            "run `deploy/upgrade.sh --yes` on main".to_owned()
        ]));
        assert!(!is_standard_upgrade_deploy_checks(&[]));
        assert!(!is_standard_upgrade_deploy_checks(&[
            "deploy/upgrade.sh --yes && curl -sf https://board.example.test/healthz".to_owned()
        ]));
        assert!(!is_standard_upgrade_deploy_checks(&[
            "deploy/upgrade.sh --yes and verify Telegram alerts work".to_owned()
        ]));
        assert!(!is_standard_upgrade_deploy_checks(&[
            "deploy/upgrade.sh --yes --dry-run".to_owned()
        ]));
        assert!(!is_standard_upgrade_deploy_checks(&[
            "deploy/upgrade.sh --yes".to_owned(),
            "inspect /fleet UI screenshot".to_owned()
        ]));
        assert!(!is_standard_upgrade_deploy_checks(&[
            "cargo test --workspace".to_owned()
        ]));
    }

    #[tokio::test]
    async fn deterministic_after_merge_deploy_waits_for_image_deploys_and_falls_back_on_error() {
        use std::sync::{Arc, Mutex};

        use board_proto::{
            AddNote, AgentRole, CreateBoard, CreateRun, CreateTask, DeviceName, Priority,
            RegisterAgent, Runtime, UpdateRun,
        };
        use board_server::{AppState, Db, ManualClock, ServerConfig};

        struct FakeGh {
            image_check: Mutex<Checks>,
        }

        impl Gh for FakeGh {
            fn view<'a>(
                &'a self,
                _project: &'a Path,
                url: &'a str,
            ) -> BoxFuture<'a, Result<PullRequest>> {
                let url = url.to_owned();
                Box::pin(async move {
                    Ok(PullRequest {
                        url,
                        state: crate::gh::PrState::Merged,
                        draft: false,
                        base_ref: "main".to_owned(),
                        head_ref: "feat/deploy-test".to_owned(),
                        head_oid: "1111111111111111111111111111111111111111".to_owned(),
                        cross_repository: false,
                        merge_commit: Some("abcdef0123456789abcdef0123456789abcdef01".to_owned()),
                        merge_state: crate::gh::MergeState::Clean,
                        review_decision: crate::gh::ReviewDecision::Approved,
                        checks: Checks::Passed,
                        passed_checks: vec!["rust".to_owned()],
                    })
                })
            }

            fn ready<'a>(&'a self, _project: &'a Path, _pr: &'a str) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }

            fn update_branch<'a>(
                &'a self,
                _project: &'a Path,
                _pr: &'a str,
            ) -> BoxFuture<'a, Result<crate::gh::Updated>> {
                Box::pin(async { Ok(crate::gh::Updated::Done) })
            }

            fn merge<'a>(
                &'a self,
                _project: &'a Path,
                _pr: &'a str,
                _head: &'a str,
            ) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }

            fn commit_check_status<'a>(
                &'a self,
                _project: &'a Path,
                _pr_url: Option<&'a str>,
                _sha: &'a str,
                _check_name: &'a str,
            ) -> BoxFuture<'a, Result<Checks>> {
                let state = self.image_check.lock().unwrap().clone();
                Box::pin(async move { Ok(state) })
            }
        }

        struct FakeDeployer {
            result: Mutex<UpgradeRunOutput>,
            calls: Mutex<Vec<String>>,
        }

        impl UpgradeDeployer for FakeDeployer {
            fn run_upgrade<'a>(
                &'a self,
                _project: &'a Path,
                sha: &'a str,
            ) -> BoxFuture<'a, Result<UpgradeRunOutput>> {
                self.calls.lock().unwrap().push(sha.to_owned());
                let out = self.result.lock().unwrap().clone();
                Box::pin(async move { Ok(out) })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("board.db");
        let db = Db::open(&db_path).unwrap();
        let now: Timestamp = "2026-11-25T12:00:00Z".parse().unwrap();
        let clock = Arc::new(ManualClock::new(now));
        let state = AppState::new(db, &ServerConfig::new(db_path), clock.clone());
        let dev: DeviceName = "mac-air".parse().unwrap();
        let token = state.create_device(&dev, true).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv_state = state.clone();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, board_server::app(srv_state)).await;
        });

        let base_url = format!("http://{addr}");
        let client_as =
            |agent: Option<AgentId>| Client::new(&base_url, token.clone(), agent, None).unwrap();
        let worker_id: AgentId = "t3:worker-1".parse().unwrap();
        let review_id: AgentId = "antigravity:rev-1".parse().unwrap();
        let dispatch_id: AgentId = "dispatch:mac-air".parse().unwrap();
        let client = client_as(Some(dispatch_id.clone()));
        let worker_client = client_as(Some(worker_id.clone()));
        let review_client = client_as(Some(review_id.clone()));
        for (c, id, role) in [
            (&worker_client, &worker_id, AgentRole::Worker),
            (&review_client, &review_id, AgentRole::Worker),
            (&client, &dispatch_id, AgentRole::Coordinator),
        ] {
            c.register(&RegisterAgent {
                id: id.clone(),
                runtime: Runtime::Antigravity,
                device: dev.clone(),
                role,
                title: None,
                repo: Some("github.com/o/r".into()),
                branch: None,
                cwd: None,
                native_ids: BTreeMap::new(),
                forked_from: None,
            })
            .await
            .unwrap();
        }
        let board = client
            .create_board(&CreateBoard {
                slug: "factory".parse().unwrap(),
                title: "Factory".into(),
                goal: "Ship".into(),
                autopilot: true,
                temporary: false,
                repos: vec!["github.com/o/r".into()],
                home_devices: vec![dev.clone()],
            })
            .await
            .unwrap();

        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let setup_task = |title: &str| {
            let client = client.clone();
            let worker_client = worker_client.clone();
            let review_client = review_client.clone();
            let worker_id = worker_id.clone();
            let review_id = review_id.clone();
            let dispatch_id = dispatch_id.clone();
            let dev = dev.clone();
            let title = title.to_owned();
            async move {
                let created = worker_client
                    .create_task(&CreateTask {
                        board: "factory".parse().unwrap(),
                        title,
                        body: "Ship change".into(),
                        acceptance: "deploy/upgrade.sh --yes".into(),
                        priority: Priority::P2,
                        parent: None,
                        blockers: vec![],
                        auto: Some(true),
                        not_before: None,
                        rank: None,
                        rank_top: false,
                        claim: true,
                        agent: worker_id.clone(),
                        repo: Some("github.com/o/r".into()),
                        weight: None,
                        resource_units: None,
                        requires: vec![],
                        tolerations: vec![],
                        conflict_keys: vec![],
                        affinity: None,
                        preferred_runner: None,
                        origin: None,
                        size: None,
                        before: None,
                        after: None,
                    })
                    .await
                    .unwrap();
                let _ = worker_client
                    .update_task(
                        &created.id,
                        &UpdateTask {
                            agent: Some(worker_id.clone().into()),
                            version: created.version,
                            status: Some(TaskStatus::Review),
                            title: None,
                            body: None,
                            acceptance: None,
                            priority: None,
                            blockers: None,
                            blocked_reason: None,
                            auto: None,
                            not_before: None,
                            rank: None,
                            owner: None,
                            reason: None,
                            repo: None,
                            weight: None,
                            resource_units: None,
                            requires: None,
                            tolerations: None,
                            conflict_keys: None,
                            affinity: None,
                            preferred_runner: None,
                            size: None,
                            cooled: None,
                            before: None,
                            after: None,
                        },
                    )
                    .await
                    .unwrap();
                // Register a completed review run so standing_review verifies the pass note.
                let rev_run = client
                    .create_run(&CreateRun {
                        task_id: Some(created.id.clone()),
                        board: Some("factory".parse().unwrap()),
                        kind: RunKind::Review,
                        origin: RunOrigin::Managed,
                        device: dev,
                        runner: "acp".into(),
                        agent_id: Some(review_id.clone()),
                        state: Some(RunState::Running),
                        lease_ttl_secs: Some(300),
                        lease_expires_at: None,
                        worktree_branch: None,
                        cwd: None,
                        title: Some("review".into()),
                        attach_hint: None,
                        cost_units: 2,
                        policy_version: None,
                        conflict_keys: vec![],
                    })
                    .await
                    .unwrap();
                let pass_note = review_client
                    .add_note(
                        &created.id,
                        &AddNote {
                            agent: review_id.clone(),
                            kind: NoteKind::Decision,
                            text: "review: pass. Clean diff.\npr: https://github.com/o/r/pull/42\nafter merge: deploy/upgrade.sh --yes".into(),
                        },
                    )
                    .await
                    .unwrap();
                let _ = client
                    .update_run(
                        &rev_run.id,
                        &UpdateRun {
                            state: Some(RunState::Succeeded),
                            outcome: Some(Some(json!({"status": "review_pass"}))),
                            ..UpdateRun::default()
                        },
                    )
                    .await
                    .unwrap();
                let ready_text = format!(
                    "{AFTER_MERGE_READY_NOTE} review pass #{}. pr: https://github.com/o/r/pull/42; merge_commit: {sha}.\n\
                     after merge: deploy/upgrade.sh --yes",
                    pass_note.id
                );
                let _ = client
                    .add_note(
                        &created.id,
                        &AddNote {
                            agent: dispatch_id.clone(),
                            kind: NoteKind::Note,
                            text: ready_text,
                        },
                    )
                    .await
                    .unwrap();
                created.id
            }
        };

        let runner = DeterministicRunner::new();
        let mut merges = BTreeMap::new();
        let project_for = |_: &Task| dir.path().to_path_buf();

        // Scenario A: `main` CI `image` check is Pending -> Deferred (deployer not invoked).
        let task_a = setup_task("Deploy happy path").await;
        let gh_pending = FakeGh {
            image_check: Mutex::new(Checks::Pending),
        };
        let dep_ok = FakeDeployer {
            result: Mutex::new(UpgradeRunOutput {
                exit_code: 0,
                output_tail: "upgraded ok".into(),
            }),
            calls: Mutex::new(Vec::new()),
        };
        let out_pending = runner
            .execute_merge_with_deployer(
                &board,
                &task_a,
                &dispatch_id,
                &client,
                &gh_pending,
                &dep_ok,
                project_for,
                &mut merges,
                now,
            )
            .await
            .unwrap();
        assert!(
            matches!(out_pending, MergeOutcome::Deferred { ref reason } if reason.contains("image")),
            "expected Deferred while image check is pending, got {out_pending:?}"
        );
        assert!(dep_ok.calls.lock().unwrap().is_empty());

        // Scenario B: `main` CI `image` check Passed + exit 0 -> review: pass + task Done!
        *gh_pending.image_check.lock().unwrap() = Checks::Passed;
        let out_done = runner
            .execute_merge_with_deployer(
                &board,
                &task_a,
                &dispatch_id,
                &client,
                &gh_pending,
                &dep_ok,
                project_for,
                &mut merges,
                now,
            )
            .await
            .unwrap();
        assert!(
            matches!(out_done, MergeOutcome::MergedAndDone { merge_commit: Some(ref c), .. } if c == sha),
            "expected MergedAndDone, got {out_done:?}"
        );
        assert_eq!(dep_ok.calls.lock().unwrap().as_slice(), &[sha.to_owned()]);
        let detail_a = client.task(&task_a).await.unwrap();
        assert_eq!(detail_a.task.status, TaskStatus::Done);
        assert!(
            detail_a
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::Decision && n.text.starts_with("review: pass.")),
            "expected review: pass decision note: {:?}",
            detail_a.notes
        );
        assert!(
            detail_a.evidence.iter().any(|e| {
                e.kind == EvidenceKind::Check
                    && e.value == format!("deploy/upgrade.sh --yes {sha}")
                    && e.exit_code == Some(0)
            }),
            "expected deploy/upgrade.sh check evidence: {:?}",
            detail_a.evidence
        );

        // Scenario C: `deploy/upgrade.sh` exits non-zero -> Trail Rule fallback finding note -> FallbackToAfterMergeJudge!
        let task_c = setup_task("Deploy failure fallback").await;
        let dep_fail = FakeDeployer {
            result: Mutex::new(UpgradeRunOutput {
                exit_code: 1,
                output_tail: "ssh connection refused".into(),
            }),
            calls: Mutex::new(Vec::new()),
        };
        let out_fail = runner
            .execute_merge_with_deployer(
                &board,
                &task_c,
                &dispatch_id,
                &client,
                &gh_pending,
                &dep_fail,
                project_for,
                &mut merges,
                now,
            )
            .await
            .unwrap();
        assert!(
            matches!(out_fail, MergeOutcome::FallbackToAfterMergeJudge { ref reason } if reason.contains("exited 1")),
            "expected FallbackToAfterMergeJudge on exit 1, got {out_fail:?}"
        );
        let detail_c = client.task(&task_c).await.unwrap();
        assert_eq!(detail_c.task.status, TaskStatus::Review);
        assert!(
            detail_c.notes.iter().any(|n| {
                n.kind == NoteKind::Finding
                    && n.text.starts_with(DET_AFTER_MERGE_FALLBACK_PREFIX)
                    && n.text.contains("ssh connection refused")
            }),
            "expected Trail Rule fallback finding note: {:?}",
            detail_c.notes
        );

        // Scenario D: Deploy Train batches multiple merged tasks into ONE `deploy/upgrade.sh --yes <tip>`
        // invocation and closes EVERY covered task with that single deploy check evidence!
        let train_t1 = setup_task("Deploy train member 1").await;
        let train_t2 = setup_task("Deploy train member 2").await;
        let dep_train = FakeDeployer {
            result: Mutex::new(UpgradeRunOutput {
                exit_code: 0,
                output_tail: "upgrade: train ok".into(),
            }),
            calls: Mutex::new(Vec::new()),
        };
        let out_train = runner
            .execute_deploy_train_with_deployer(
                &board,
                &[train_t1.clone(), train_t2.clone()],
                &dispatch_id,
                &client,
                &gh_pending,
                &dep_train,
                project_for,
                &mut merges,
                now,
            )
            .await
            .unwrap();
        assert!(
            matches!(out_train, MergeOutcome::MergedAndDone { merge_commit: Some(ref c), .. } if c == sha),
            "expected MergedAndDone for deploy train, got {out_train:?}"
        );
        assert_eq!(
            dep_train.calls.lock().unwrap().len(),
            1,
            "deploy train must invoke upgrade.sh exactly ONCE for both tasks"
        );
        for tid in [&train_t1, &train_t2] {
            let d = client.task(tid).await.unwrap();
            assert_eq!(d.task.status, TaskStatus::Done, "{tid} must be done");
            assert!(
                d.evidence.iter().any(|e| {
                    e.kind == EvidenceKind::Check
                        && e.value == format!("deploy/upgrade.sh --yes {sha}")
                        && e.exit_code == Some(0)
                }),
                "{tid} must carry shared deploy train check evidence: {:?}",
                d.evidence
            );
        }

        state.shutdown();
        server_handle.abort();
    }
}
