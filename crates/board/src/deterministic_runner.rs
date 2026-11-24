//! Zero-LLM deterministic runner for `RunKind::Merge` and `RunKind::Housekeeping`
//! (`docs/SCHEDULER_DESIGN.md`, Section 4.3).
//!
//! Executes verified pull-request merges, `pr: none` / already-merged task completions,
//! gone-asker answer delivery, and away-owner task releases natively in Rust with zero LLM
//! tokens and zero agent process spawning.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, bail};
use board_proto::{
    AgentId, AgentSource, AgentState, Board, DoneTask, ErrorCode, EvidenceKind, HousekeepingAction,
    NewEvidence, Note, NoteKind, Run, RunFilter, RunKind, RunOrigin, RunState, Task, TaskDetail,
    TaskId, TaskStatus, Timestamp, UpdateTask,
};
use serde_json::{Value, json};

use crate::client::{Client, error_code};
use crate::daemon::judges::{
    AdvanceOutcome, DispatchAskOutcome, JUDGE_NOTE, Merge, PassedPr, ReviewVerdict, advance_pr,
    handle_dispatch_ask_answer, invalidates_review_pass, parked_in_review_at, passed_checks,
    passed_pr, passed_summary, review_verdict,
};
use crate::daemon::{Worker, add_note, clip_note};
use crate::gh::{Gh, PullRequest};

/// Runner kind identifier advertised to `POST /v1/dispatch/claim` and device heartbeats.
pub const DETERMINISTIC_RUNNER_KIND: &str = "deterministic";

/// Outcome of one [`DeterministicRunner::execute_merge`] pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// PR was merged (or `pr: none` / already merged) and `task_done` succeeded.
    MergedAndDone {
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
        let merged = match passed {
            PassedPr::None => None,
            PassedPr::Unclear(why) => {
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

        self.finish_task(agent, client, task, pass, merged.as_ref())
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

    async fn finish_task(
        &self,
        agent: &AgentId,
        client: &Client,
        task: &Task,
        pass: &Note,
        merged: Option<&PullRequest>,
    ) -> Result<()> {
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
        evidence.extend(passed_checks(&pass.text));
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
                        agent: agent.clone(),
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
}
