//! Zero-LLM deterministic runner for `RunKind::Merge` and `RunKind::Housekeeping`
//! (`docs/SCHEDULER_DESIGN.md`, Section 4.3).
//!
//! Executes verified pull-request merges, `pr: none` / already-merged task completions,
//! gone-asker answer delivery, and away-owner task releases natively in Rust with zero LLM
//! tokens and zero agent process spawning.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use board_proto::{
    AgentId, AgentSource, AgentState, Board, DoneTask, ErrorCode, EvidenceKind, HousekeepingAction,
    NewEvidence, Note, NoteKind, Run, RunFilter, RunKind, RunOrigin, RunState, Task, TaskDetail,
    TaskId, TaskStatus, Timestamp, UpdateTask,
};
use futures_util::future::{BoxFuture, join_all, try_join_all};
use serde_json::{Value, json};

use crate::client::{Client, error_code};
use crate::daemon::judges::{
    AFTER_MERGE_READY_NOTE, AdvanceOutcome, DispatchAskOutcome, JUDGE_NOTE, Merge, PR_HOLD_WAIT,
    PassedPr, ReviewVerdict, advance_pr, after_merge_checks, after_merge_loop_detected,
    after_merge_ready_info, check_shared_pr_merge_gate, collect_pass_checks,
    format_after_merge_ready_note, handle_dispatch_ask_answer, has_after_merge_ready_note_after,
    invalidates_review_pass, parked_in_review_at, passed_pr, passed_summary, reopen_passed_task,
    review_verdict,
};
use crate::daemon::{Worker, add_note, clip_note, reopen_held_task};
use crate::gh::{Checks, Gh, PrState, PullRequest};

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

/// Trail Rule `note` prefix posted when deterministic `deploy/upgrade.sh --yes --skip-unreachable`
/// upgrades the server and reachable devices while leaving out unreachable SSH device(s)
/// (exit 3), recording the timestamp, actor, skipped host(s), and catch-up command.
pub const DET_AFTER_MERGE_SKIPPED_PREFIX: &str =
    "note: deterministic after-merge deploy skipped unreachable device(s)";

/// Line prefix printed by `deploy/upgrade.sh --skip-unreachable` on stdout when one or more
/// non-server devices could not be reached over SSH (exit 3).
const SKIPPED_SUMMARY_PREFIX: &str = "left out because ssh could not connect:";

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

/// Production [`UpgradeDeployer`] invoking
/// `<project>/deploy/upgrade.sh --yes --skip-unreachable --no-restart-local <sha>`.
///
/// Passing `--skip-unreachable` lets `upgrade.sh` upgrade the server and reachable devices
/// when a secondary device is asleep/offline over SSH (exiting 3 with the skipped host list
/// and catch-up command, while server or non-SSH device failures still exit 1).
///
/// Passing `--no-restart-local` prevents `upgrade.sh` from killing the calling `board watch`
/// daemon mid-run before `DeterministicRunner` finishes `task_done` and marks the run
/// `succeeded`. The daemon triggers a detached graceful self-restart afterwards when the
/// installed binary commit actually changed.
#[derive(Debug, Clone, Default)]
pub struct RealUpgradeDeployer;

fn tail_chars(text: &str, max_chars: usize) -> String {
    text.chars()
        .rev()
        .take(max_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// Parsed summary of a `deploy/upgrade.sh --yes --skip-unreachable <sha>` exit-3 run where
/// the server and reachable devices upgraded while one or more non-server devices could not
/// be reached over SSH.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkippedUpgradeInfo {
    skipped_hosts: Vec<String>,
    catch_up_command: String,
}

fn push_unique_host(hosts: &mut Vec<String>, candidate: &str) {
    let host = candidate.trim().trim_end_matches('.');
    if !host.is_empty() && !hosts.iter().any(|h| h == host) {
        hosts.push(host.to_owned());
    }
}

fn parse_skipped_upgrade_output(output: &str, sha: &str) -> SkippedUpgradeInfo {
    let mut skipped_hosts = Vec::new();
    let mut catch_up_command = String::new();

    if let Some(line) = output
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with(SKIPPED_SUMMARY_PREFIX))
    {
        let rest = line
            .trim_start()
            .strip_prefix(SKIPPED_SUMMARY_PREFIX)
            .unwrap_or("")
            .trim();
        let hosts_part = if let Some((hosts, cmd)) = rest.split_once(". Once they answer, run:") {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                catch_up_command = cmd.to_owned();
            }
            hosts
        } else {
            rest.trim_end_matches('.')
        };
        for host in hosts_part.split_ascii_whitespace() {
            push_unique_host(&mut skipped_hosts, host);
        }
    }

    if skipped_hosts.is_empty() {
        for line in output.lines() {
            let trimmed = line.trim();
            if let Some(prefix) = trimmed.strip_suffix(": left out (ssh could not connect)") {
                let host = prefix.strip_prefix("device ").unwrap_or(prefix).trim();
                push_unique_host(&mut skipped_hosts, host);
            } else if let Some(rest) = trimmed.strip_prefix("upgrade: ssh could not connect to ")
                && let Some(host) = rest.split_ascii_whitespace().next()
            {
                push_unique_host(&mut skipped_hosts, host);
            }
        }
    }

    if catch_up_command.is_empty() {
        catch_up_command = format!("deploy/upgrade.sh --yes {sha}");
    }

    SkippedUpgradeInfo {
        skipped_hosts,
        catch_up_command,
    }
}

fn format_skipped_hosts(hosts: &[String]) -> String {
    if hosts.is_empty() {
        "unknown (see deploy output)".to_owned()
    } else {
        hosts.join(", ")
    }
}

fn format_skipped_deploy_trail_note(
    now: Timestamp,
    agent: &AgentId,
    sha: &str,
    train_ids: Option<&[String]>,
    info: &SkippedUpgradeInfo,
) -> String {
    let hosts = format_skipped_hosts(&info.skipped_hosts);
    let scope = match train_ids {
        Some(ids) => format!("train [{}] at {sha}", ids.join(", ")),
        None => format!("commit {sha}"),
    };
    clip_note(format!(
        "{DET_AFTER_MERGE_SKIPPED_PREFIX} ({hosts}) at {} by {agent} for {scope}: \
         server and reachable devices upgraded (deploy/upgrade.sh --yes --skip-unreachable {sha} \
         exited 3; {} skipped host(s)). Catch-up once awake: {}",
        now.to_rfc3339(),
        info.skipped_hosts.len(),
        info.catch_up_command,
    ))
}

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
                .arg("--skip-unreachable")
                .arg("--no-restart-local")
                .arg(sha)
                .current_dir(project)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| {
                    format!(
                        "spawning {} --yes --skip-unreachable --no-restart-local {sha}",
                        script.display()
                    )
                })?;
            let Ok(out) =
                tokio::time::timeout(DEPLOY_UPGRADE_TIMEOUT, child.wait_with_output()).await
            else {
                bail!(
                    "deploy/upgrade.sh --yes --skip-unreachable --no-restart-local {sha} timed out after {}s",
                    DEPLOY_UPGRADE_TIMEOUT.as_secs()
                );
            };
            let out = out.context("waiting for deploy/upgrade.sh")?;
            let exit_code = out.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let output_tail = if exit_code == 3 {
                // On exit 3 (`--skip-unreachable`), `deploy/upgrade.sh` prints the `done:`
                // summary and `left out because ssh could not connect: ...` line to stdout
                // while `cargo install` can emit large progress logs to stderr. Preserve the
                // stdout summary so skipped hosts and the catch-up command are never truncated
                // by stderr noise.
                let trimmed = stdout.trim();
                if trimmed.is_empty() {
                    tail_chars(stderr.trim(), 600)
                } else if let Some(summary_line) = trimmed
                    .lines()
                    .rev()
                    .find(|line| line.trim_start().starts_with(SKIPPED_SUMMARY_PREFIX))
                    && summary_line.chars().count() > 600
                {
                    summary_line.trim().to_owned()
                } else {
                    tail_chars(trimmed, 600)
                }
            } else {
                let combined = format!("{stdout}\n{stderr}");
                tail_chars(combined.trim(), 400)
            };
            Ok(UpgradeRunOutput {
                exit_code,
                output_tail,
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

/// Picks the index of the newest descendant commit in `shas` (ordered oldest-to-newest by
/// `ready_at`). When multiple entries share the tip SHA, keeps the highest-index (newest)
/// entry so its `pr_url` is preserved.
async fn select_deploy_train_tip(project: &Path, shas: &[&str]) -> usize {
    let mut tip_idx = shas.len() - 1;
    if shas.len() <= 1 {
        return tip_idx;
    }
    let initial_sha = shas[tip_idx];
    let checks = join_all((0..tip_idx).map(|idx| {
        let candidate_sha = shas[idx];
        async move {
            if candidate_sha == initial_sha {
                (idx, false)
            } else {
                (
                    idx,
                    git_is_ancestor(project, initial_sha, candidate_sha).await,
                )
            }
        }
    }))
    .await;
    let descendants: Vec<usize> = checks
        .into_iter()
        .filter_map(|(idx, is_desc)| is_desc.then_some(idx))
        .collect();
    if let Some(&first_desc) = descendants.first() {
        tip_idx = first_desc;
        for &idx in &descendants[1..] {
            if shas[tip_idx] == shas[idx]
                || git_is_ancestor(project, shas[tip_idx], shas[idx]).await
            {
                tip_idx = idx;
            }
        }
    }
    tip_idx
}

/// Outcome of one [`DeterministicRunner::execute_merge`] pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// PR was merged (or `pr: none` / already merged) and `task_done` succeeded.
    MergedAndDone {
        pr_url: Option<String>,
        merge_commit: Option<String>,
        /// Non-server devices skipped over SSH during deterministic `--skip-unreachable`
        /// deploy (exit 3), if any.
        skipped_hosts: Vec<String>,
        /// Catch-up command to run once skipped host(s) wake up, if any.
        catch_up_command: Option<String>,
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
    /// updated, or worker still running). The run parks in `waiting_external` (releasing
    /// merge/deploy conflict locks immediately) and is re-claimed in place on a future tick
    /// instead of inserting a fresh `runs` row on every poll.
    Deferred {
        reason: String,
        pr_head_sha: Option<String>,
    },
    /// Task is no longer in `review` or no longer has a standing pass.
    Noop { reason: String },
    /// Pass note names an ambiguous or foreign PR URL that cannot be merged automatically.
    Unclear { reason: String },
    /// Deterministic `deploy/upgrade.sh` after-merge deploy could not complete; a Trail Rule
    /// `finding` note was written so the server falls back to the LLM `AfterMerge` judge.
    FallbackToAfterMergeJudge { reason: String },
}

/// Reason returned when the task is already merged and handed off to an LLM after-merge
/// review judge (so the merge run itself should complete as `succeeded`, not wait in
/// `waiting_external`).
const WAITING_FOR_AFTER_MERGE_JUDGE_REASON: &str =
    "already merged; waiting for after-merge review judge";

impl MergeOutcome {
    /// Whether this pass deferred waiting on GitHub checks or a running worker.
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred { .. })
    }

    /// Whether this deferred pass should park the existing `Merge` run in
    /// `waiting_external` for coalesced in-place re-polling (rather than closing the run
    /// as `succeeded`).
    pub fn should_wait_external(&self) -> bool {
        matches!(
            self,
            Self::Deferred { reason, .. } if reason != WAITING_FOR_AFTER_MERGE_JUDGE_REASON
        )
    }

    /// Reason text when [`Self::Deferred`].
    pub fn deferred_reason(&self) -> Option<&str> {
        match self {
            Self::Deferred { reason, .. } => Some(reason.as_str()),
            _ => None,
        }
    }

    /// Structured JSON outcome recorded on `PATCH /v1/runs/{id}`.
    pub fn to_outcome_json(&self) -> Value {
        match self {
            Self::MergedAndDone {
                pr_url,
                merge_commit,
                skipped_hosts,
                catch_up_command,
            } => {
                let mut obj = json!({
                    "status": "merged_and_done",
                    "pr_url": pr_url,
                    "merge_commit": merge_commit,
                });
                if !skipped_hosts.is_empty() {
                    obj["skipped_hosts"] = json!(skipped_hosts);
                }
                if let Some(cmd) = catch_up_command {
                    obj["catch_up_command"] = json!(cmd);
                }
                obj
            }
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
            Self::Deferred {
                reason,
                pr_head_sha,
            } => {
                let mut obj = json!({
                    "status": "deferred",
                    "reason": reason,
                });
                if let Some(sha) = pr_head_sha {
                    obj["pr_head_sha"] = json!(sha);
                }
                obj
            }
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

    /// Structured JSON outcome enriched with coalesced poll metadata (`poll_count`,
    /// `first_deferred_at`, `last_polled_at`, and `prev_pr_head_sha` when a new commit
    /// arrived on the PR) derived from the run's previous outcome JSON.
    ///
    /// When `pr_head_sha` changes compared to the previous deferred poll, `poll_count`
    /// resets to `1` so server-side backoff drops back to the initial fast interval (60s)
    /// for the newly pushed commit.
    pub fn to_outcome_json_with_poll(&self, prev_outcome: Option<&Value>, now: Timestamp) -> Value {
        let mut out = self.to_outcome_json();
        let prev_poll_count = prev_outcome
            .and_then(|v| v.get("poll_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let prev_sha = prev_outcome
            .and_then(|v| v.get("pr_head_sha"))
            .and_then(Value::as_str);
        let prev_first = prev_outcome
            .and_then(|v| v.get("first_deferred_at"))
            .and_then(Value::as_str);

        if let Self::Deferred { pr_head_sha, .. } = self {
            let sha_changed = match (prev_sha, pr_head_sha.as_deref()) {
                (Some(old), Some(new)) => old != new,
                _ => false,
            };
            let poll_count = if sha_changed {
                1
            } else {
                prev_poll_count.saturating_add(1)
            };
            let now_rfc3339 = now.to_rfc3339();
            let first_deferred_at = if sha_changed {
                now_rfc3339.clone()
            } else {
                prev_first
                    .map(str::to_owned)
                    .unwrap_or_else(|| now_rfc3339.clone())
            };
            out["poll_count"] = json!(poll_count);
            out["first_deferred_at"] = json!(first_deferred_at);
            out["last_polled_at"] = json!(now_rfc3339);
            if sha_changed && let Some(old) = prev_sha {
                out["prev_pr_head_sha"] = json!(old);
            }
        } else if prev_poll_count > 0 {
            out["poll_count"] = json!(prev_poll_count.saturating_add(1));
            if let Some(first) = prev_first {
                out["first_deferred_at"] = json!(first);
            }
        }
        out
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

        let detail_results = join_all(
            task_ids
                .iter()
                .map(|tid| async move { (tid, client.task(tid).await) }),
        )
        .await;
        let mut review_details: Vec<TaskDetail> = Vec::with_capacity(task_ids.len());
        for (tid, res) in detail_results {
            let detail = match res {
                Ok(d) => d,
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => continue,
                Err(err) => return Err(err.context(format!("reading {tid} for deploy train"))),
            };
            if detail.task.status == TaskStatus::Review {
                review_details.push(detail);
            }
        }

        let runs_results = join_all(review_details.iter().map(|detail| async move {
            let filter = RunFilter {
                task: Some(detail.task.id.clone()),
                kind: Some(RunKind::Review),
                origin: Some(RunOrigin::Managed),
                limit: Some(50),
                ..RunFilter::default()
            };
            client.list_runs(&filter).await
        }))
        .await;

        struct EligibleMember {
            detail: TaskDetail,
            pass: Note,
            pr_url: Option<String>,
            merge_commit: Option<String>,
            ready_at: Timestamp,
        }

        let mut eligible: Vec<EligibleMember> = Vec::with_capacity(review_details.len());
        let mut project: Option<std::path::PathBuf> = None;
        for (detail, runs_res) in review_details.into_iter().zip(runs_results) {
            let tid = detail.task.id.clone();
            let review_runs = match runs_res {
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
            project.get_or_insert_with(|| resolve_project(&detail.task));
            let info = after_merge_ready_info(&detail, pass.id);
            let pr_url = info.as_ref().and_then(|i| i.pr_url.clone()).or_else(|| {
                match passed_pr(&detail, &pass, &board.repos) {
                    PassedPr::Url(url) => Some(url),
                    _ => None,
                }
            });
            let merge_commit = info.as_ref().and_then(|i| i.merge_commit.clone());
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
            eligible.push(EligibleMember {
                detail,
                pass,
                pr_url,
                merge_commit,
                ready_at,
            });
        }

        let Some(proj) = project else {
            return Ok(MergeOutcome::Noop {
                reason: "no deploy-eligible tasks remained in deploy train".to_owned(),
            });
        };

        let gh_views = join_all(eligible.iter().map(|m| {
            let proj_ref = proj.as_path();
            async move {
                if m.merge_commit
                    .as_deref()
                    .is_none_or(|c| !is_full_hex_sha(c))
                    && let Some(ref url) = m.pr_url
                {
                    gh.view(proj_ref, url).await.ok()
                } else {
                    None
                }
            }
        }))
        .await;

        let mut members: Vec<TrainMember> = Vec::with_capacity(eligible.len());
        let mut missing_sha_ids: Vec<TaskId> = Vec::new();
        for (mut m, pr_view) in eligible.into_iter().zip(gh_views) {
            if let Some(ref pr) = pr_view
                && let Some(ref mc) = pr.merge_commit
                && is_full_hex_sha(mc)
            {
                m.merge_commit = Some(mc.clone());
            }
            let Some(sha) = m.merge_commit.filter(|c| is_full_hex_sha(c)) else {
                missing_sha_ids.push(m.detail.task.id);
                continue;
            };
            members.push(TrainMember {
                detail: m.detail,
                pass: m.pass,
                pr_url: m.pr_url,
                sha,
                pr_view,
                ready_at: m.ready_at,
            });
        }

        if !missing_sha_ids.is_empty() {
            let reason =
                "no full 40-character merge commit SHA available for deploy/upgrade.sh".to_owned();
            let text = clip_note(format!(
                "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
            ));
            try_join_all(
                missing_sha_ids
                    .iter()
                    .map(|tid| add_note(client, tid, agent, NoteKind::Finding, text.clone())),
            )
            .await?;
        }

        if members.is_empty() {
            return Ok(MergeOutcome::Noop {
                reason: "no deploy-eligible tasks remained in deploy train".to_owned(),
            });
        }

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
        let tip_idx = if is_git_repo && members.len() > 1 {
            let shas: Vec<&str> = members.iter().map(|m| m.sha.as_str()).collect();
            select_deploy_train_tip(&proj, &shas).await
        } else {
            members.len() - 1
        };
        let tip_sha = members[tip_idx].sha.clone();
        let tip_pr_url = members[tip_idx].pr_url.clone();

        // Keep only tasks whose merge commit is covered by `tip_sha` (or all members when
        // running against a non-git test directory with a fake deployer).
        let covered: Vec<TrainMember> = if !is_git_repo {
            members
        } else {
            let proj_ref = proj.as_path();
            let tip_ref = tip_sha.as_str();
            join_all(members.into_iter().map(|m| async move {
                let ok = m.sha == tip_ref || git_is_ancestor(proj_ref, &m.sha, tip_ref).await;
                (ok, m)
            }))
            .await
            .into_iter()
            .filter_map(|(ok, m)| ok.then_some(m))
            .collect()
        };
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
                    try_join_all(covered.iter().map(|m| {
                        add_note(
                            client,
                            &m.detail.task.id,
                            agent,
                            NoteKind::Finding,
                            text.clone(),
                        )
                    }))
                    .await?;
                    return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
                }
                return Ok(MergeOutcome::Deferred {
                    reason: format!(
                        "waiting for main CI `image` job on deploy train tip {tip_sha}"
                    ),
                    pr_head_sha: Some(tip_sha),
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
                try_join_all(covered.iter().map(|m| {
                    add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Finding,
                        text.clone(),
                    )
                }))
                .await?;
                return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
            }
            Err(err) => {
                let reason =
                    format!("reading main CI `image` status for {tip_sha} failed: {err:#}");
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                try_join_all(covered.iter().map(|m| {
                    add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Finding,
                        text.clone(),
                    )
                }))
                .await?;
                return Ok(MergeOutcome::FallbackToAfterMergeJudge { reason });
            }
        }

        // Execute `deploy/upgrade.sh --yes --skip-unreachable --no-restart-local <tip_sha>` ONCE
        // for the whole train.
        let train_ids: Vec<String> = covered
            .iter()
            .map(|m| m.detail.task.id.to_string())
            .collect();
        let check_cmd = format!("deploy/upgrade.sh --yes --skip-unreachable {tip_sha}");
        match deployer.run_upgrade(&proj, &tip_sha).await {
            Ok(out) if out.exit_code == 0 || out.exit_code == 3 => {
                let exit_code = out.exit_code;
                let skipped = (exit_code == 3)
                    .then(|| parse_skipped_upgrade_output(&out.output_tail, &tip_sha));
                let covered_len = covered.len();
                let outcomes = join_all(covered.iter().map(|m| {
                    let tip_sha_ref = tip_sha.as_str();
                    let check_cmd_ref = check_cmd.as_str();
                    let skipped_ref = skipped.as_ref();
                    let train_ids_ref = train_ids.as_slice();
                    async move {
                        let fresh_detail = match client.task(&m.detail.task.id).await {
                            Ok(d) => d,
                            Err(_) => m.detail.clone(),
                        };
                        if fresh_detail.task.status == TaskStatus::Done {
                            return Ok(m.detail.task.id.clone());
                        }
                        if let Some(info) = skipped_ref {
                            let trail_text = format_skipped_deploy_trail_note(
                                now,
                                agent,
                                tip_sha_ref,
                                Some(train_ids_ref),
                                info,
                            );
                            if let Err(err) = add_note(
                                client,
                                &m.detail.task.id,
                                agent,
                                NoteKind::Note,
                                trail_text,
                            )
                            .await
                            {
                                tracing::warn!(
                                    "{}: deploy train {tip_sha_ref} exited 3, but recording Trail Rule skip note failed: {err:#}",
                                    m.detail.task.id
                                );
                                return Err(format!("{}: {err:#}", m.detail.task.id));
                            }
                        }
                        let mut extra = vec![NewEvidence {
                            kind: EvidenceKind::Check,
                            value: check_cmd_ref.to_owned(),
                            exit_code: Some(exit_code),
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
                                "{}: deploy train {tip_sha_ref} succeeded (exit {exit_code}), but closing task failed: {err:#}",
                                m.detail.task.id
                            );
                            return Err(format!("{}: {err:#}", m.detail.task.id));
                        }
                        let pr_line = match &m.pr_url {
                            Some(url) => format!("pr: {url}"),
                            None => "pr: none".to_owned(),
                        };
                        let pass_text = match skipped_ref {
                            Some(info) => {
                                let hosts = format_skipped_hosts(&info.skipped_hosts);
                                clip_note(format!(
                                    "review: pass. Deployed in {covered_len}-task deploy train at {tip_sha_ref} \
                                     (commit {}) to server and reachable devices \
                                     (skipped unreachable SSH device(s): {hosts}; catch-up: {}).\n\
                                     check: {check_cmd_ref} => 3\n\
                                     {pr_line}",
                                    m.sha, info.catch_up_command
                                ))
                            }
                            None => clip_note(format!(
                                "review: pass. Deployed in {covered_len}-task deploy train at {tip_sha_ref} (commit {}).\n\
                                 check: {check_cmd_ref} => 0\n\
                                 {pr_line}",
                                m.sha
                            )),
                        };
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
                                "{}: deployed in train {tip_sha_ref}, but recording trailing pass note failed: {err:#}",
                                m.detail.task.id
                            );
                        }
                        Ok(m.detail.task.id.clone())
                    }
                }))
                .await;
                let mut closed = 0usize;
                let mut close_errors: Vec<String> = Vec::new();
                for outcome in outcomes {
                    match outcome {
                        Ok(tid) => {
                            closed += 1;
                            merges.remove(&tid);
                        }
                        Err(err) => close_errors.push(err),
                    }
                }
                if closed == 0 && !close_errors.is_empty() {
                    bail!(
                        "deploy train at {tip_sha} succeeded (exit {exit_code}), but closing all covered tasks failed: {}",
                        close_errors.join("; ")
                    );
                }
                if let Some(ref info) = skipped {
                    tracing::info!(
                        "deploy train finished {closed}/{} task(s) [{}] at {tip_sha} (exit 3; skipped unreachable SSH device(s): {})",
                        covered.len(),
                        train_ids.join(", "),
                        format_skipped_hosts(&info.skipped_hosts)
                    );
                } else {
                    tracing::info!(
                        "deploy train finished {closed}/{} task(s) [{}] at {tip_sha}",
                        covered.len(),
                        train_ids.join(", ")
                    );
                }
                Ok(MergeOutcome::MergedAndDone {
                    pr_url: tip_pr_url,
                    merge_commit: Some(tip_sha),
                    skipped_hosts: skipped
                        .as_ref()
                        .map(|i| i.skipped_hosts.clone())
                        .unwrap_or_default(),
                    catch_up_command: skipped.map(|i| i.catch_up_command),
                })
            }
            Ok(out) => {
                let tail = if out.output_tail.is_empty() {
                    "no output".to_owned()
                } else {
                    out.output_tail
                };
                let reason = format!(
                    "{check_cmd} (train [{}]) exited {}: {tail}",
                    train_ids.join(", "),
                    out.exit_code
                );
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                try_join_all(covered.iter().map(|m| {
                    add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Finding,
                        text.clone(),
                    )
                }))
                .await?;
                Ok(MergeOutcome::FallbackToAfterMergeJudge { reason })
            }
            Err(err) => {
                let reason = format!(
                    "{check_cmd} (train [{}]) error: {err:#}",
                    train_ids.join(", ")
                );
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                try_join_all(covered.iter().map(|m| {
                    add_note(
                        client,
                        &m.detail.task.id,
                        agent,
                        NoteKind::Finding,
                        text.clone(),
                    )
                }))
                .await?;
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
        let runs_filter = RunFilter {
            task: Some(task_id.clone()),
            kind: Some(RunKind::Review),
            origin: Some(RunOrigin::Managed),
            limit: Some(50),
            ..RunFilter::default()
        };
        let (detail_res, runs_res) =
            tokio::join!(client.task(task_id), client.list_runs(&runs_filter));
        let detail = match detail_res {
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
        let review_runs = match runs_res {
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
                reason: WAITING_FOR_AFTER_MERGE_JUDGE_REASON.to_owned(),
                pr_head_sha: None,
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
                    pr_head_sha: None,
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
                if pr.state == PrState::Open
                    && let Some(reason) = check_shared_pr_merge_gate(
                        board,
                        agent,
                        client,
                        &detail,
                        pass,
                        &url,
                        &pr,
                        &BTreeSet::new(),
                    )
                    .await?
                {
                    merge.clear_hold();
                    merges.insert(task.id.clone(), merge);
                    return Ok(MergeOutcome::Deferred {
                        reason,
                        pr_head_sha: Some(pr.head_oid),
                    });
                }
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
                    AdvanceOutcome::Deferred { reason, head_sha } => {
                        return Ok(MergeOutcome::Deferred {
                            reason,
                            pr_head_sha: head_sha,
                        });
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
                    skipped_hosts: Vec::new(),
                    catch_up_command: None,
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
                    skipped_hosts: Vec::new(),
                    catch_up_command: None,
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
                reopen_held_task(client, agent, &detail.task)
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
                let (detail_res, info_res) =
                    tokio::join!(client.task(task_id), client.agent_info(owner));
                let detail = match detail_res {
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
                let info = match info_res {
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
                    pr_head_sha: Some(sha),
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

        // Execute `deploy/upgrade.sh --yes --skip-unreachable --no-restart-local <sha>`.
        let check_cmd = format!("deploy/upgrade.sh --yes --skip-unreachable {sha}");
        match deployer.run_upgrade(project, &sha).await {
            Ok(out) if out.exit_code == 0 || out.exit_code == 3 => {
                let exit_code = out.exit_code;
                let skipped =
                    (exit_code == 3).then(|| parse_skipped_upgrade_output(&out.output_tail, &sha));
                if let Some(ref info) = skipped {
                    // Trail Rule: record skipped host(s) and catch-up command BEFORE closing.
                    let trail_text = format_skipped_deploy_trail_note(now, agent, &sha, None, info);
                    add_note(client, &task.id, agent, NoteKind::Note, trail_text).await?;
                }
                let mut extra = vec![NewEvidence {
                    kind: EvidenceKind::Check,
                    value: check_cmd.clone(),
                    exit_code: Some(exit_code),
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
                let pass_text = match skipped.as_ref() {
                    Some(info) => {
                        let hosts = format_skipped_hosts(&info.skipped_hosts);
                        clip_note(format!(
                            "review: pass. Deployed merged commit {sha} to server and reachable devices \
                             via deterministic after-merge runner (skipped unreachable SSH device(s): {hosts}; \
                             catch-up: {}).\n\
                             check: {check_cmd} => 3\n\
                             {pr_line}",
                            info.catch_up_command
                        ))
                    }
                    None => clip_note(format!(
                        "review: pass. Deployed merged commit {sha} via deterministic after-merge runner.\n\
                         check: {check_cmd} => 0\n\
                         {pr_line}"
                    )),
                };
                if let Err(err) =
                    add_note(client, &task.id, agent, NoteKind::Decision, pass_text).await
                {
                    tracing::warn!(
                        "{}: deployed and finished, but recording trailing deploy pass note failed: {err:#}",
                        task.id
                    );
                }
                merges.remove(&task.id);
                if let Some(ref info) = skipped {
                    tracing::info!(
                        "{}: deployed {sha} and finished via deterministic after-merge runner (exit 3; skipped unreachable SSH device(s): {})",
                        task.id,
                        format_skipped_hosts(&info.skipped_hosts)
                    );
                } else {
                    tracing::info!(
                        "{}: deployed {sha} and finished via deterministic after-merge runner",
                        task.id
                    );
                }
                Ok(MergeOutcome::MergedAndDone {
                    pr_url,
                    merge_commit: Some(sha),
                    skipped_hosts: skipped
                        .as_ref()
                        .map(|i| i.skipped_hosts.clone())
                        .unwrap_or_default(),
                    catch_up_command: skipped.map(|i| i.catch_up_command),
                })
            }
            Ok(out) => {
                let tail = if out.output_tail.is_empty() {
                    "no output".to_owned()
                } else {
                    out.output_tail
                };
                let reason = format!("{check_cmd} exited {}: {tail}", out.exit_code);
                let text = clip_note(format!(
                    "{DET_AFTER_MERGE_FALLBACK_PREFIX} ({reason}). Falling back to LLM after-merge review judge."
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                Ok(MergeOutcome::FallbackToAfterMergeJudge { reason })
            }
            Err(err) => {
                let reason = format!("{check_cmd} error: {err:#}");
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
                        expedited: None,
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
                        expedited: None,
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
                            expedited: None,
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
            matches!(out_pending, MergeOutcome::Deferred { ref reason, .. } if reason.contains("image")),
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
                    && e.value == format!("deploy/upgrade.sh --yes --skip-unreachable {sha}")
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
                        && e.value == format!("deploy/upgrade.sh --yes --skip-unreachable {sha}")
                        && e.exit_code == Some(0)
                }),
                "{tid} must carry shared deploy train check evidence: {:?}",
                d.evidence
            );
        }

        // Scenario E: Single-task exit 3 (non-server SSH device unreachable) -> upgrades
        // reachable hosts, records Trail Rule skip note BEFORE closing, posts `review: pass`
        // naming skipped host(s) and catch-up command, and closes task with exit 3 check.
        let task_e = setup_task("Deploy with sleeping device").await;
        let catch_up_e =
            format!("/repo/deploy/upgrade.sh --config /etc/agent-board/upgrade.conf --yes {sha}");
        let dep_skip = FakeDeployer {
            result: Mutex::new(UpgradeRunOutput {
                exit_code: 3,
                output_tail: format!(
                    "done: the server runs abcdef012345; device revisions:\n\
                       local: abcdef012345 (was 111111111111)\n\
                       laptop-sleep: left out (ssh could not connect)\n\
                     left out because ssh could not connect: laptop-sleep. Once they answer, run: {catch_up_e}"
                ),
            }),
            calls: Mutex::new(Vec::new()),
        };
        let out_skip = runner
            .execute_merge_with_deployer(
                &board,
                &task_e,
                &dispatch_id,
                &client,
                &gh_pending,
                &dep_skip,
                project_for,
                &mut merges,
                now,
            )
            .await
            .unwrap();
        assert_eq!(
            out_skip,
            MergeOutcome::MergedAndDone {
                pr_url: Some("https://github.com/o/r/pull/42".to_owned()),
                merge_commit: Some(sha.to_owned()),
                skipped_hosts: vec!["laptop-sleep".to_owned()],
                catch_up_command: Some(catch_up_e.clone()),
            }
        );
        let out_skip_json = out_skip.to_outcome_json();
        assert_eq!(out_skip_json["skipped_hosts"], json!(["laptop-sleep"]));
        assert_eq!(out_skip_json["catch_up_command"], json!(catch_up_e));
        let detail_e = client.task(&task_e).await.unwrap();
        assert_eq!(detail_e.task.status, TaskStatus::Done);
        let trail_note_e = detail_e
            .notes
            .iter()
            .find(|n| {
                n.kind == NoteKind::Note && n.text.starts_with(DET_AFTER_MERGE_SKIPPED_PREFIX)
            })
            .expect("expected Trail Rule skipped-device note on task_e");
        assert!(
            trail_note_e.text.contains("laptop-sleep")
                && trail_note_e.text.contains(&catch_up_e)
                && trail_note_e.text.contains(&now.to_rfc3339())
                && trail_note_e.text.contains(dispatch_id.as_str()),
            "Trail Rule note must name skipped host, catch-up cmd, timestamp, and actor: {}",
            trail_note_e.text
        );
        let pass_note_e = detail_e
            .notes
            .iter()
            .rev()
            .find(|n| {
                n.kind == NoteKind::Decision
                    && n.agent == dispatch_id
                    && n.text.starts_with("review: pass.")
            })
            .expect("expected dispatcher review: pass note on task_e");
        assert!(
            pass_note_e.text.contains("laptop-sleep")
                && pass_note_e.text.contains(&catch_up_e)
                && pass_note_e.text.contains(&format!(
                    "check: deploy/upgrade.sh --yes --skip-unreachable {sha} => 3"
                )),
            "review: pass note must record skipped host, catch-up cmd, and exit 3 check: {}",
            pass_note_e.text
        );
        assert!(
            trail_note_e.id < pass_note_e.id,
            "Trail Rule note must be written before closing/review: pass"
        );
        assert!(
            !detail_e
                .notes
                .iter()
                .any(|n| n.text.starts_with(DET_AFTER_MERGE_FALLBACK_PREFIX)),
            "exit 3 must not post fallback finding note"
        );
        assert!(
            detail_e.evidence.iter().any(|e| {
                e.kind == EvidenceKind::Check
                    && e.value == format!("deploy/upgrade.sh --yes --skip-unreachable {sha}")
                    && e.exit_code == Some(3)
            }),
            "expected exit 3 check evidence on task_e: {:?}",
            detail_e.evidence
        );

        // Scenario F: Deploy train exit 3 with multiple sleeping hosts upgrades reachable hosts
        // once and records Trail Rule skip note + review: pass + exit 3 check on EVERY member.
        let train_skip_1 = setup_task("Deploy train skip 1").await;
        let train_skip_2 = setup_task("Deploy train skip 2").await;
        let dep_train_skip = FakeDeployer {
            result: Mutex::new(UpgradeRunOutput {
                exit_code: 3,
                output_tail: format!(
                    "left out because ssh could not connect: laptop-a laptop-b.ts.net. \
                     Once they answer, run: {catch_up_e}"
                ),
            }),
            calls: Mutex::new(Vec::new()),
        };
        let out_train_skip = runner
            .execute_deploy_train_with_deployer(
                &board,
                &[train_skip_1.clone(), train_skip_2.clone()],
                &dispatch_id,
                &client,
                &gh_pending,
                &dep_train_skip,
                project_for,
                &mut merges,
                now,
            )
            .await
            .unwrap();
        assert_eq!(dep_train_skip.calls.lock().unwrap().len(), 1);
        assert!(matches!(
            out_train_skip,
            MergeOutcome::MergedAndDone {
                ref skipped_hosts,
                catch_up_command: Some(ref cmd),
                ..
            } if skipped_hosts == &["laptop-a", "laptop-b.ts.net"] && cmd == &catch_up_e
        ));
        for tid in [&train_skip_1, &train_skip_2] {
            let d = client.task(tid).await.unwrap();
            assert_eq!(d.task.status, TaskStatus::Done, "{tid} must be done");
            assert!(
                d.notes.iter().any(|n| {
                    n.kind == NoteKind::Note
                        && n.text.starts_with(DET_AFTER_MERGE_SKIPPED_PREFIX)
                        && n.text.contains("laptop-a, laptop-b.ts.net")
                        && n.text.contains(&catch_up_e)
                }),
                "{tid} must carry Trail Rule skip note: {:?}",
                d.notes
            );
            assert!(
                d.notes.iter().any(|n| {
                    n.kind == NoteKind::Decision
                        && n.agent == dispatch_id
                        && n.text.starts_with("review: pass.")
                        && n.text.contains("laptop-a, laptop-b.ts.net")
                        && n.text.contains("=> 3")
                }),
                "{tid} must carry review: pass note naming skipped hosts: {:?}",
                d.notes
            );
            assert!(
                d.evidence.iter().any(|e| {
                    e.kind == EvidenceKind::Check
                        && e.value == format!("deploy/upgrade.sh --yes --skip-unreachable {sha}")
                        && e.exit_code == Some(3)
                }),
                "{tid} must carry exit 3 check evidence: {:?}",
                d.evidence
            );
        }

        // Scenario G: Deploy train exit 1 (server or non-SSH device failure) still fails
        // deterministic deploy and falls back to LLM judge on every covered task.
        let train_fail_1 = setup_task("Deploy train fail 1").await;
        let train_fail_2 = setup_task("Deploy train fail 2").await;
        let dep_train_fail = FakeDeployer {
            result: Mutex::new(UpgradeRunOutput {
                exit_code: 1,
                output_tail: "upgrade: the server upgrade stopped (see above)".into(),
            }),
            calls: Mutex::new(Vec::new()),
        };
        let out_train_fail = runner
            .execute_deploy_train_with_deployer(
                &board,
                &[train_fail_1.clone(), train_fail_2.clone()],
                &dispatch_id,
                &client,
                &gh_pending,
                &dep_train_fail,
                project_for,
                &mut merges,
                now,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                out_train_fail,
                MergeOutcome::FallbackToAfterMergeJudge { ref reason }
                    if reason.contains("exited 1")
            ),
            "expected FallbackToAfterMergeJudge on train exit 1, got {out_train_fail:?}"
        );
        for tid in [&train_fail_1, &train_fail_2] {
            let d = client.task(tid).await.unwrap();
            assert_eq!(
                d.task.status,
                TaskStatus::Review,
                "{tid} must remain in review"
            );
            assert!(
                d.notes.iter().any(|n| {
                    n.kind == NoteKind::Finding
                        && n.text.starts_with(DET_AFTER_MERGE_FALLBACK_PREFIX)
                }),
                "{tid} must carry fallback finding note: {:?}",
                d.notes
            );
        }

        state.shutdown();
        server_handle.abort();
    }

    #[tokio::test]
    async fn select_deploy_train_tip_picks_newest_descendant_and_preserves_tie_order() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };

        run_git(&["init", "-b", "main"]);
        std::fs::write(repo.join("f.txt"), "1\n").unwrap();
        run_git(&["add", "f.txt"]);
        run_git(&["commit", "-m", "c1"]);
        let c1 = run_git(&["rev-parse", "HEAD"]);

        std::fs::write(repo.join("f.txt"), "2\n").unwrap();
        run_git(&["commit", "-am", "c2"]);
        let c2 = run_git(&["rev-parse", "HEAD"]);

        std::fs::write(repo.join("f.txt"), "3\n").unwrap();
        run_git(&["commit", "-am", "c3"]);
        let c3 = run_git(&["rev-parse", "HEAD"]);

        // Duplicate tip SHA: must keep the newest (last) index rather than demoting to 0.
        assert_eq!(select_deploy_train_tip(repo, &[&c3, &c3]).await, 1);

        // Out-of-order ready_at where an earlier slot holds the newest descendant commit.
        assert_eq!(select_deploy_train_tip(repo, &[&c3, &c2, &c1]).await, 0);

        // Multiple strict descendants of the last slot: picks the newest descendant, and on
        // duplicate descendants picks the later (newest ready_at) slot.
        assert_eq!(
            select_deploy_train_tip(repo, &[&c2, &c3, &c3, &c1]).await,
            2
        );

        // Already sorted ascending: stays on the last index.
        assert_eq!(select_deploy_train_tip(repo, &[&c1, &c2, &c3]).await, 2);
    }

    #[test]
    fn parse_skipped_upgrade_output_extracts_hosts_and_catch_up_command() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";

        let full = format!(
            "done: the server runs abcdef012345; device revisions:\n\
               local: abcdef012345 (was 111111111111)\n\
               laptop-a: left out (ssh could not connect)\n\
               laptop-b.ts.net: left out (ssh could not connect)\n\
             left out because ssh could not connect: laptop-a laptop-b.ts.net. \
             Once they answer, run: /opt/repo/deploy/upgrade.sh --config /home/u/.config/agent-board/upgrade.conf --yes {sha}"
        );
        let parsed = parse_skipped_upgrade_output(&full, sha);
        assert_eq!(
            parsed.skipped_hosts,
            vec!["laptop-a".to_owned(), "laptop-b.ts.net".to_owned()]
        );
        assert_eq!(
            parsed.catch_up_command,
            format!(
                "/opt/repo/deploy/upgrade.sh --config /home/u/.config/agent-board/upgrade.conf --yes {sha}"
            )
        );

        let stderr_only = "upgrade: ssh could not connect to box-2 (see above): leaving it out (--skip-unreachable)\n";
        let parsed_stderr = parse_skipped_upgrade_output(stderr_only, sha);
        assert_eq!(parsed_stderr.skipped_hosts, vec!["box-2".to_owned()]);
        assert_eq!(
            parsed_stderr.catch_up_command,
            format!("deploy/upgrade.sh --yes {sha}")
        );

        let empty = parse_skipped_upgrade_output("", sha);
        assert!(empty.skipped_hosts.is_empty());
        assert_eq!(
            empty.catch_up_command,
            format!("deploy/upgrade.sh --yes {sha}")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_upgrade_deployer_passes_skip_unreachable_and_preserves_exit_3_summary() {
        let dir = tempfile::tempdir().unwrap();
        let deploy_dir = dir.path().join("deploy");
        std::fs::create_dir_all(&deploy_dir).unwrap();
        let args_file = dir.path().join("args.txt");
        let script = deploy_dir.join("upgrade.sh");
        crate::test_script::write_script(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s' \"$*\" > '{}'\necho ok\n",
                args_file.display()
            ),
        );
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let out = RealUpgradeDeployer
            .run_upgrade(dir.path(), sha)
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(&args_file).unwrap(),
            format!("--yes --skip-unreachable --no-restart-local {sha}")
        );

        // On exit 3, heavy stderr (such as `cargo install` progress output) must NOT
        // truncate stdout's `left out because ssh could not connect: ...` summary line.
        let noisy_stderr = "Compiling board v0.1.0\n".repeat(40);
        let catch_up = format!(
            "{} --config /etc/agent-board/upgrade.conf --yes {sha}",
            script.display()
        );
        crate::test_script::write_script(
            &script,
            &format!(
                "#!/bin/sh\n\
                 printf '%s' '{noisy_stderr}' >&2\n\
                 echo 'done: the server runs abcdef012345; device revisions:'\n\
                 echo '  local: abcdef012345 (was 111111111111)'\n\
                 echo '  asleep-mac: left out (ssh could not connect)'\n\
                 echo 'left out because ssh could not connect: asleep-mac. Once they answer, run: {catch_up}'\n\
                 exit 3\n"
            ),
        );
        let out3 = RealUpgradeDeployer
            .run_upgrade(dir.path(), sha)
            .await
            .unwrap();
        assert_eq!(out3.exit_code, 3);
        let parsed = parse_skipped_upgrade_output(&out3.output_tail, sha);
        assert_eq!(parsed.skipped_hosts, vec!["asleep-mac".to_owned()]);
        assert_eq!(parsed.catch_up_command, catch_up);
    }
}
