//! Best-effort deterministic context pre-packing for daemon-dispatched workers and judges
//! (`RunKind::Worker`, `RunKind::Triage`, `RunKind::Verdict`, `RunKind::Review`, and
//! `RunKind::Goal`).
//!
//! Prefetches task details, PR metadata + bounded git diffs, board goals, compact
//! duplicate-detection indices, and matching `setups.toml` entries in Rust before launching
//! the session so workers and judges can act on Turn 1 without burning extra turns on
//! redundant `task_show`, `board_status`, or unfiltered `task_list` calls.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

use board_proto::{
    Board, BoardSlug, FeedbackActivity, FeedbackSource, Note, NoteKind, Priority, ResolutionKind,
    TASKS_LIMIT_MAX, Task, TaskDetail, TaskId, TaskStatus,
};

use crate::client::{Client, TaskQuery};
use crate::gh::{Checks, MergeState, PrState, PullRequest, ReviewDecision};

const MAX_TRIAGE_CANDIDATES: usize = 25;
const MAX_VERDICT_INDEX: usize = 30;
const MAX_GOAL_DONE_ITEMS: usize = 15;
const MAX_GOAL_OPEN_ITEMS: usize = 20;
const MAX_PLAIN_NOTES_BATCH: usize = 4;
const MAX_PLAIN_NOTES_REVIEW: usize = 6;
const MAX_PLAIN_NOTE_CHARS: usize = 450;
/// Full server note length (800 chars) so `decision` and `finding` notes are never clipped.
const MAX_KEY_NOTE_CHARS: usize = 800;

/// Soft character budget for the pre-packed body before `brief::append_prepacked_context`'s
/// hard `PREPACKED_CONTEXT_CHARS_MAX` (10,000) outer fence, reserving space for trailing
/// sections (candidates, `setups.toml`, board index, children) so tail truncation never
/// silently drops them.
const SOFT_TOTAL_BUDGET: usize = 9_400;
const TRIAGE_CANDIDATES_BUDGET: usize = 2_200;
const TRIAGE_SETUPS_BUDGET: usize = 1_500;
const VERDICT_INDEX_BUDGET: usize = 2_500;
const VERDICT_GOAL_BUDGET: usize = 1_000;
const GOAL_TEXT_BUDGET: usize = 2_000;
const GOAL_DONE_BUDGET: usize = 4_200;
const GOAL_OPEN_BUDGET: usize = 2_400;
const REVIEW_CHILDREN_BUDGET: usize = 1_200;
const REVIEW_ACCEPTANCE_BUDGET: usize = 1_200;
const REVIEW_BODY_BUDGET: usize = 2_200;
const WORKER_CHILDREN_BUDGET: usize = 1_000;
const WORKER_ACCEPTANCE_BUDGET: usize = 1_400;
const WORKER_BODY_BUDGET: usize = 2_600;
const REVIEW_DIFF_STAT_BUDGET: usize = 2_000;
/// Upper bound on pre-packed unified diff bytes for `RunKind::Review` (`24 KiB`).
pub(crate) const REVIEW_DIFF_MAX_BYTES: usize = 24 * 1024;
const GIT_DIFF_TIMEOUT: Duration = Duration::from_secs(10);

/// Best-effort pre-packed context for a `RunKind::Triage` judge run.
pub(crate) async fn build_triage_context(
    client: &Client,
    board: &BoardSlug,
    tasks: &[TaskId],
    project: &Path,
) -> String {
    let mut details = Vec::with_capacity(tasks.len());
    for tid in tasks {
        match client.task(tid).await {
            Ok(detail) => details.push(detail),
            Err(err) => tracing::debug!("triage prepack: fetching {tid} failed: {err:#}"),
        }
    }

    let abouts: BTreeSet<String> = details
        .iter()
        .filter_map(|d| d.task.feedback.as_ref().map(|f| f.about.clone()))
        .collect();

    let candidates = if !abouts.is_empty() {
        fetch_triage_candidates(client, board).await
    } else {
        Vec::new()
    };

    let setups_text = std::fs::read_to_string(project.join("setups.toml")).ok();
    format_triage_context(&details, &candidates, setups_text.as_deref())
}

/// Best-effort pre-packed context for a `RunKind::Verdict` judge run (including `board.goal`).
pub(crate) async fn build_verdict_context(
    client: &Client,
    board: &BoardSlug,
    board_goal: &str,
    tasks: &[TaskId],
) -> String {
    let mut details = Vec::with_capacity(tasks.len());
    for tid in tasks {
        match client.task(tid).await {
            Ok(detail) => details.push(detail),
            Err(err) => tracing::debug!("verdict prepack: fetching {tid} failed: {err:#}"),
        }
    }

    let board_tasks = match client
        .tasks(&TaskQuery {
            board: Some(board.clone()),
            closed: Some(false),
            limit: Some(TASKS_LIMIT_MAX),
            ..TaskQuery::default()
        })
        .await
    {
        Ok(list) => list,
        Err(err) => {
            tracing::debug!("verdict prepack: listing open tasks on {board} failed: {err:#}");
            Vec::new()
        }
    };

    format_verdict_context(board_goal, &details, &board_tasks)
}

/// Best-effort pre-packed context for a `RunKind::Goal` judge run: includes `board.goal`,
/// current counts, recently closed tasks, and open/blocked tasks.
pub(crate) async fn build_goal_context(client: &Client, board: &Board) -> String {
    // `GET /v1/tasks` returns tasks in priority + number ascending order, not newest-first,
    // so fetch up to `TASKS_LIMIT_MAX` before sorting by `updated_at DESC` / status locally.
    let done_query = TaskQuery {
        board: Some(board.slug.clone()),
        status: Some(TaskStatus::Done),
        limit: Some(TASKS_LIMIT_MAX),
        ..TaskQuery::default()
    };
    let open_query = TaskQuery {
        board: Some(board.slug.clone()),
        closed: Some(false),
        limit: Some(TASKS_LIMIT_MAX),
        ..TaskQuery::default()
    };
    let (done_res, open_res) = tokio::join!(client.tasks(&done_query), client.tasks(&open_query));
    let recent_done = match done_res {
        Ok(list) => list,
        Err(err) => {
            tracing::debug!(
                "goal prepack: listing done tasks on {} failed: {err:#}",
                board.slug
            );
            Vec::new()
        }
    };
    let open_tasks = match open_res {
        Ok(list) => list,
        Err(err) => {
            tracing::debug!(
                "goal prepack: listing open tasks on {} failed: {err:#}",
                board.slug
            );
            Vec::new()
        }
    };
    format_goal_context(board, &recent_done, &open_tasks)
}

/// Pure formatter for `RunKind::Goal` pre-packed context (unit-testable without network I/O).
pub(crate) fn format_goal_context(
    board: &Board,
    recent_done: &[Task],
    open_tasks: &[Task],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "### Board {} ({})",
        board.slug,
        one_line(&board.title, 120)
    );
    let c = &board.counts;
    let _ = writeln!(
        out,
        "counts: open {} · doing {} · blocked {} · review {} · done {} · ready {} (auto {})",
        c.open,
        c.doing,
        c.blocked,
        c.review,
        c.done,
        c.ready,
        c.ready_auto.unwrap_or(0)
    );
    if !board.goal.trim().is_empty() {
        let (goal, cut) = truncate_chars_flag(board.goal.trim(), GOAL_TEXT_BUDGET);
        let _ = writeln!(out, "\n### Board goal\n{goal}");
        if cut {
            let _ = writeln!(
                out,
                "(goal truncated; run `board status --board {} --detailed` if needed)",
                board.slug
            );
        }
    }

    let done_maybe_cut = recent_done.len() >= TASKS_LIMIT_MAX as usize;
    let mut done_sorted: Vec<&Task> = recent_done.iter().collect();
    done_sorted.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.number().cmp(&a.id.number()))
    });
    done_sorted.truncate(MAX_GOAL_DONE_ITEMS);
    if !done_sorted.is_empty() {
        let mut section =
            format!("\n### Recently finished tasks (up to {MAX_GOAL_DONE_ITEMS}, newest first)\n");
        for task in done_sorted {
            let summary = task
                .summary
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| format!(" — {}", one_line(s, 160)))
                .unwrap_or_default();
            let line = format!(
                "- {} [{}] {}{summary}\n",
                task.id,
                priority_str(task.priority),
                one_line(&task.title, 120),
            );
            if section.chars().count() + line.chars().count() > GOAL_DONE_BUDGET {
                break;
            }
            section.push_str(&line);
        }
        if done_maybe_cut {
            let _ = writeln!(
                section,
                "(done list reached the {TASKS_LIMIT_MAX}-task page limit; run `board ls --board {} --status done --since <date>` if newer finished tasks may be omitted)",
                board.slug
            );
        }
        out.push_str(&section);
    }

    let mut open_sorted: Vec<&Task> = open_tasks
        .iter()
        .filter(|t| !matches!(t.status, TaskStatus::Done | TaskStatus::Dropped))
        .collect();
    open_sorted.sort_by(|a, b| {
        status_rank(a.status)
            .cmp(&status_rank(b.status))
            .then_with(|| a.priority.cmp(&b.priority))
            .then_with(|| a.id.cmp(&b.id))
    });
    open_sorted.truncate(MAX_GOAL_OPEN_ITEMS);
    if !open_sorted.is_empty() {
        let mut section = format!("\n### Open/active tasks (up to {MAX_GOAL_OPEN_ITEMS})\n");
        for task in open_sorted {
            let tag = if task.proposal { " proposal" } else { "" };
            let line = format!(
                "- {} [{} {}{tag}] {}\n",
                task.id,
                status_str(task.status),
                priority_str(task.priority),
                one_line(&task.title, 120),
            );
            if section.chars().count() + line.chars().count() > GOAL_OPEN_BUDGET {
                break;
            }
            section.push_str(&line);
        }
        out.push_str(&section);
    }

    out.trim().to_owned()
}

/// Formats an already-fetched [`TaskDetail`] for a `RunKind::Worker` run so the worker can
/// act on Turn 1 without redundant `task_show` calls.
pub(crate) fn format_worker_context(
    detail: &TaskDetail,
    branch: Option<&str>,
    repo: Option<&str>,
) -> String {
    let mut out = String::new();
    let task = &detail.task;
    let _ = writeln!(
        out,
        "### {} [{}] ({}) {}",
        task.id,
        priority_str(task.priority),
        status_str(task.status),
        one_line(&task.title, 160),
    );
    if let Some(repo) = repo.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "repo: {repo}");
    }
    if let Some(branch) = branch.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = writeln!(out, "branch: {branch}");
    }
    if let Some(parent) = &task.parent {
        let _ = writeln!(out, "parent: {parent}");
    }
    if !task.blockers.is_empty() {
        let list: Vec<&str> = task.blockers.iter().map(TaskId::as_str).collect();
        let _ = writeln!(out, "blockers: {}", list.join(", "));
    }
    if !detail.blocks.is_empty() {
        let list: Vec<&str> = detail.blocks.iter().map(TaskId::as_str).collect();
        let _ = writeln!(out, "blocks: {}", list.join(", "));
    }
    if let Some(reason) = task
        .blocked_reason
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        let _ = writeln!(out, "blocked_reason: {}", one_line(reason, 200));
    }

    let children_section = format_children_section(detail, WORKER_CHILDREN_BUDGET);

    let mut body_truncated = false;
    if !task.body.trim().is_empty() {
        let (body, cut) = truncate_chars_flag(task.body.trim(), WORKER_BODY_BUDGET);
        body_truncated = cut;
        let _ = writeln!(out, "\n#### Body\n{body}");
    }
    if !task.acceptance.trim().is_empty() {
        let (acc, cut) = truncate_chars_flag(task.acceptance.trim(), WORKER_ACCEPTANCE_BUDGET);
        body_truncated |= cut;
        let _ = writeln!(out, "\n#### Acceptance\n{acc}");
    }

    let remaining_for_notes = SOFT_TOTAL_BUDGET
        .saturating_sub(out.chars().count())
        .saturating_sub(children_section.chars().count())
        .saturating_sub(120);
    write_notes_preserving_key_kinds(
        &mut out,
        &task.id,
        &detail.notes,
        &detail.handles,
        MAX_PLAIN_NOTES_REVIEW,
        remaining_for_notes,
    );

    if body_truncated {
        let _ = writeln!(
            out,
            "(body/acceptance truncated; run `board show {} --detailed` if full text is needed)",
            task.id
        );
    }
    if !children_section.is_empty() {
        out.push_str(&children_section);
    }
    out.trim().to_owned()
}

/// Best-effort pre-packed context for a pre-merge `RunKind::Review` judge: combines the
/// task detail, `gh.view` PR metadata (URL, head SHA, CI check summary), `git diff --stat`,
/// and a bounded unified diff (`<= 24 KiB`) read from `git_dir`.
pub(crate) async fn build_review_context(
    detail: &TaskDetail,
    pr: Option<&PullRequest>,
    git_dir: &Path,
    worktree_base: Option<&str>,
) -> String {
    let (diff_stat, unified_diff) = collect_review_git_diff(git_dir, pr, worktree_base).await;
    format_review_context_with_pr(detail, pr, diff_stat.as_deref(), unified_diff.as_deref())
}

/// Formats the already-fetched [`TaskDetail`] for a `RunKind::Review` judge run (without PR
/// diff extras, e.g. for `AfterMerge` reviews or unit tests).
pub(crate) fn format_review_context(detail: &TaskDetail) -> String {
    format_review_context_with_pr(detail, None, None, None)
}

/// Pure formatter for a `RunKind::Review` judge run, including optional PR metadata,
/// `git diff --stat`, and bounded unified diff (`<= 24 KiB`).
pub(crate) fn format_review_context_with_pr(
    detail: &TaskDetail,
    pr: Option<&PullRequest>,
    diff_stat: Option<&str>,
    unified_diff: Option<&str>,
) -> String {
    let mut out = String::new();
    let task = &detail.task;
    let owner = task
        .owner
        .as_ref()
        .map(|a| resolve_handle(&detail.handles, a.as_str()))
        .unwrap_or("unowned");
    let _ = writeln!(
        out,
        "### {} [{}] ({}) owner={} {}",
        task.id,
        priority_str(task.priority),
        status_str(task.status),
        owner,
        one_line(&task.title, 160),
    );
    if let Some(parent) = &task.parent {
        let _ = writeln!(out, "parent: {parent}");
    }
    if !task.blockers.is_empty() {
        let list: Vec<&str> = task.blockers.iter().map(TaskId::as_str).collect();
        let _ = writeln!(out, "blockers: {}", list.join(", "));
    }
    if !detail.blocks.is_empty() {
        let list: Vec<&str> = detail.blocks.iter().map(TaskId::as_str).collect();
        let _ = writeln!(out, "blocks: {}", list.join(", "));
    }
    if let Some(reason) = task
        .blocked_reason
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        let _ = writeln!(out, "blocked_reason: {}", one_line(reason, 200));
    }

    // Build trailing children section first so we know its exact size.
    let children_section = format_children_section(detail, REVIEW_CHILDREN_BUDGET);

    let mut body_truncated = false;
    if !task.body.trim().is_empty() {
        let (body, cut) = truncate_chars_flag(task.body.trim(), REVIEW_BODY_BUDGET);
        body_truncated = cut;
        let _ = writeln!(out, "\n#### Body\n{body}");
    }
    if !task.acceptance.trim().is_empty() {
        let (acc, cut) = truncate_chars_flag(task.acceptance.trim(), REVIEW_ACCEPTANCE_BUDGET);
        body_truncated |= cut;
        let _ = writeln!(out, "\n#### Acceptance\n{acc}");
    }

    let remaining_for_notes = SOFT_TOTAL_BUDGET
        .saturating_sub(out.chars().count())
        .saturating_sub(children_section.chars().count())
        .saturating_sub(120);
    write_notes_preserving_key_kinds(
        &mut out,
        &task.id,
        &detail.notes,
        &detail.handles,
        MAX_PLAIN_NOTES_REVIEW,
        remaining_for_notes,
    );

    if body_truncated {
        let _ = writeln!(
            out,
            "(body/acceptance truncated; run `board show {} --detailed` if full text is needed)",
            task.id
        );
    }
    if !children_section.is_empty() {
        out.push_str(&children_section);
    }

    if let Some(pr) = pr {
        let _ = writeln!(out, "\n#### Pull request");
        let _ = writeln!(out, "url: {}", pr.url);
        let _ = writeln!(
            out,
            "state: {} | draft: {} | base: {} | head: {}",
            pr_state_str(pr.state),
            pr.draft,
            pr.base_ref,
            pr.head_ref,
        );
        if !pr.head_oid.is_empty() {
            let _ = writeln!(out, "headRefOid: {}", pr.head_oid);
        }
        if let Some(mc) = pr.merge_commit.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(out, "mergeCommit: {mc}");
        }
        let _ = writeln!(
            out,
            "merge_state: {} | review: {} | checks: {}",
            merge_state_str(pr.merge_state),
            review_decision_str(pr.review_decision),
            checks_summary(&pr.checks, &pr.passed_checks),
        );
    }

    if let Some(stat) = diff_stat.map(str::trim).filter(|s| !s.is_empty()) {
        let (stat_cut, truncated) = truncate_chars_flag(stat, REVIEW_DIFF_STAT_BUDGET);
        let _ = writeln!(out, "\n#### Diff stat\n```text\n{stat_cut}\n```");
        if truncated {
            let _ = writeln!(
                out,
                "(diff stat truncated; run `git diff --stat` if needed)"
            );
        }
    }

    if let Some(diff) = unified_diff.map(str::trim).filter(|s| !s.is_empty()) {
        // Reserve room for `append_prepacked_context_with_limit`'s outer fence, our diff code
        // fence, and the truncation warning so the outer wrapper never slices off the closing
        // fence or truncation notice.
        const REVIEW_WRAPPER_AND_FENCE_OVERHEAD: usize = 600;
        let max_diff_chars = crate::brief::REVIEW_PREPACKED_CONTEXT_CHARS_MAX
            .saturating_sub(REVIEW_WRAPPER_AND_FENCE_OVERHEAD)
            .saturating_sub(out.chars().count());
        let (byte_cut, byte_truncated) = truncate_bytes_utf8(diff, REVIEW_DIFF_MAX_BYTES);
        let (diff_cut, char_truncated) = truncate_chars_flag(byte_cut, max_diff_chars);
        let truncated = byte_truncated || char_truncated;
        if !diff_cut.is_empty() {
            let _ = writeln!(out, "\n#### Unified diff\n```diff\n{diff_cut}\n```");
            if truncated {
                let _ = writeln!(
                    out,
                    "(unified diff truncated at 24 KiB; run `gh pr diff` or `git diff` if needed)"
                );
            }
        }
    }

    out.trim().to_owned()
}

/// Best-effort read of `git diff --stat` and unified `git diff` from `git_dir` for a
/// pre-merge review judge.
pub(crate) async fn collect_review_git_diff(
    git_dir: &Path,
    pr: Option<&PullRequest>,
    worktree_base: Option<&str>,
) -> (Option<String>, Option<String>) {
    let candidates: Vec<String> = if let Some(pr) = pr {
        if pr.state == PrState::Merged {
            pr.merge_commit
                .as_deref()
                .filter(|c| !c.is_empty())
                .map(|c| vec![format!("{c}^1..{c}")])
                .unwrap_or_default()
        } else if !pr.base_ref.is_empty() && !pr.head_ref.is_empty() {
            let mut ranges = Vec::new();
            if !pr.head_oid.is_empty() {
                ranges.push(format!("origin/{}...{}", pr.base_ref, pr.head_oid));
            }
            let remote_head = format!("origin/{}...origin/{}", pr.base_ref, pr.head_ref);
            if !ranges.contains(&remote_head) {
                ranges.push(remote_head);
            }
            if let Some(base) = worktree_base.filter(|b| !b.is_empty()) {
                let alt = format!("origin/{}...{base}", pr.base_ref);
                if !ranges.contains(&alt) {
                    ranges.push(alt);
                }
            }
            ranges
        } else {
            Vec::new()
        }
    } else if let Some(head) = worktree_base.filter(|b| !b.is_empty()) {
        vec![
            format!("origin/HEAD...{head}"),
            format!("origin/main...{head}"),
        ]
    } else {
        Vec::new()
    };

    for range in candidates {
        let Some(stat) = run_git_diff(
            git_dir,
            &["diff", "--stat", &range],
            REVIEW_DIFF_STAT_BUDGET * 4,
        )
        .await
        else {
            continue;
        };
        let unified = run_git_diff(git_dir, &["diff", &range], REVIEW_DIFF_MAX_BYTES).await;
        if stat.trim().is_empty() && unified.as_deref().is_none_or(|d| d.trim().is_empty()) {
            return (None, None);
        }
        return (
            (!stat.trim().is_empty()).then_some(stat),
            unified.filter(|d| !d.trim().is_empty()),
        );
    }
    (None, None)
}

/// Runs `git` and streams at most `max_bytes + 1` bytes from stdout so multi-MB diffs
/// never buffer in memory or time out. The extra byte lets callers detect truncation.
async fn run_git_diff(git_dir: &Path, args: &[&str], max_bytes: usize) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--no-optional-locks")
        .arg("-C")
        .arg(git_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let limit = max_bytes.saturating_add(1);
    let mut buf = Vec::with_capacity(limit.min(32 * 1024));
    let read_res = tokio::time::timeout(
        GIT_DIFF_TIMEOUT,
        stdout.take(limit as u64).read_to_end(&mut buf),
    )
    .await;
    if read_res.ok()?.is_err() {
        let _ = child.kill().await;
        return None;
    }
    if buf.len() >= limit {
        // Hit the stream cap; kill git so we don't wait for the rest of a huge diff.
        let _ = child.kill().await;
        return Some(String::from_utf8_lossy(&buf).into_owned());
    }
    let status = tokio::time::timeout(GIT_DIFF_TIMEOUT, child.wait())
        .await
        .ok()?
        .ok()?;
    if !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn pr_state_str(state: PrState) -> &'static str {
    match state {
        PrState::Open => "open",
        PrState::Merged => "merged",
        PrState::Closed => "closed",
    }
}

fn merge_state_str(state: MergeState) -> &'static str {
    match state {
        MergeState::Clean => "clean",
        MergeState::Behind => "behind",
        MergeState::Conflict => "conflict",
        MergeState::Waits => "waits",
    }
}

fn review_decision_str(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::None => "none",
        ReviewDecision::Approved => "approved",
        ReviewDecision::ReviewRequired => "review_required",
        ReviewDecision::ChangesRequested => "changes_requested",
    }
}

fn checks_summary(checks: &Checks, passed_checks: &[String]) -> String {
    match checks {
        Checks::Passed if passed_checks.is_empty() => {
            "none (no passing CI checks reported)".to_owned()
        }
        Checks::Passed => format!("passed ({})", passed_checks.join(", ")),
        Checks::Pending => "pending".to_owned(),
        Checks::Failed(names) if names.is_empty() => "failed".to_owned(),
        Checks::Failed(names) => format!("failed ({})", names.join(", ")),
    }
}

fn truncate_bytes_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn format_children_section(detail: &TaskDetail, budget: usize) -> String {
    let mut section = String::new();
    if !detail.child_tasks.is_empty() {
        let _ = writeln!(section, "\n#### Children ({})", detail.child_tasks.len());
        for (shown, child) in detail.child_tasks.iter().enumerate() {
            let line = format!(
                "- {} [{}] {}\n",
                child.id,
                status_str(child.status),
                one_line(&child.title, 120)
            );
            if section.chars().count() + line.chars().count() + 48 > budget {
                let omitted = detail.child_tasks.len() - shown;
                let _ = writeln!(section, "... ({omitted} more children)");
                break;
            }
            section.push_str(&line);
        }
    } else if !detail.children.is_empty() {
        let list: Vec<&str> = detail.children.iter().map(TaskId::as_str).collect();
        let (joined, _) = truncate_chars_flag(&list.join(", "), budget.saturating_sub(24));
        let _ = writeln!(section, "\n#### Children\n{joined}");
    }
    section
}

async fn fetch_triage_candidates(client: &Client, board: &BoardSlug) -> Vec<Task> {
    let live_query = TaskQuery {
        board: Some(board.clone()),
        live: Some(true),
        limit: Some(TASKS_LIMIT_MAX),
        ..TaskQuery::default()
    };
    match client.tasks(&live_query).await {
        Ok(tasks) => tasks,
        Err(err) => {
            tracing::debug!(
                "triage prepack: live list failed ({err:#}); falling back to open list"
            );
            let fallback = TaskQuery {
                board: Some(board.clone()),
                closed: Some(false),
                limit: Some(TASKS_LIMIT_MAX),
                ..TaskQuery::default()
            };
            client.tasks(&fallback).await.unwrap_or_default()
        }
    }
}

/// Pure formatter for triage pre-packed context (unit-testable without network I/O).
///
/// Budgets trailing lookup sections (`Same-about candidates` and `Matching setups.toml
/// entries`) first so every item in a multi-item batch plus both lookup sections fit inside
/// [`SOFT_TOTAL_BUDGET`] without tail truncation.
pub(crate) fn format_triage_context(
    details: &[TaskDetail],
    candidates: &[Task],
    setups_toml: Option<&str>,
) -> String {
    let mut batch_ids = BTreeSet::new();
    let mut abouts = BTreeSet::new();
    for detail in details {
        batch_ids.insert(detail.task.id.clone());
        if let Some(fb) = &detail.task.feedback {
            abouts.insert(fb.about.clone());
        }
    }

    // Build trailing lookup sections first within bounded budgets so they are never cut off.
    let candidates_section = if !abouts.is_empty() {
        format_triage_candidates_section(candidates, &batch_ids, &abouts, TRIAGE_CANDIDATES_BUDGET)
    } else {
        String::new()
    };
    let setups_section = if let (false, Some(raw_toml)) = (abouts.is_empty(), setups_toml) {
        let extracted = extract_matching_setups(raw_toml, &abouts);
        if extracted.is_empty() {
            String::new()
        } else {
            let (clipped, _) = truncate_chars_flag(&extracted, TRIAGE_SETUPS_BUDGET);
            format!("### Matching setups.toml entries\n{clipped}\n")
        }
    } else {
        String::new()
    };

    let reserved = candidates_section.chars().count() + setups_section.chars().count();
    let items_budget = SOFT_TOTAL_BUDGET.saturating_sub(reserved);
    let per_item_budget = if details.is_empty() {
        items_budget
    } else {
        (items_budget / details.len()).max(500)
    };

    let mut out = String::new();
    for detail in details {
        let item_start = out.chars().count();
        let task = &detail.task;
        let _ = writeln!(
            out,
            "### {} [{}] ({}) {}",
            task.id,
            priority_str(task.priority),
            status_str(task.status),
            one_line(&task.title, 160),
        );
        if let Some(fb) = &task.feedback {
            let _ = write!(
                out,
                "about: {} | source: {} | reports: {} | signature: {}",
                fb.about,
                feedback_source_str(fb.source),
                fb.reports,
                one_line(&fb.signature, 120),
            );
            if let Some(activity) = fb.activity {
                let _ = write!(out, " | activity: {}", feedback_activity_str(activity));
            }
            if let Some(recurs) = &fb.recurs {
                let _ = write!(out, " | recurs: {recurs}");
            }
            out.push('\n');
            if !fb.reporters.is_empty() {
                let shown: Vec<&str> = fb
                    .reporters
                    .iter()
                    .rev()
                    .take(5)
                    .map(|a| resolve_handle(&detail.handles, a.as_str()))
                    .collect();
                let _ = writeln!(
                    out,
                    "reporters ({}): {}",
                    fb.reporters.len(),
                    shown.join(", ")
                );
            }
            if let Some(res) = &fb.resolution {
                let _ = write!(out, "resolution: {}", resolution_str(res.kind));
                if let Some(t) = &res.task {
                    let _ = write!(out, " -> {t}");
                }
                if let Some(r) = &res.reason {
                    let _ = write!(out, " ({})", one_line(r, 140));
                }
                out.push('\n');
            }
        }

        let header_used = out.chars().count().saturating_sub(item_start);
        let item_rem = per_item_budget.saturating_sub(header_used);
        let body_cap = if detail.notes.is_empty() {
            item_rem.min(1_800)
        } else {
            (item_rem / 2).clamp(250, 1_400)
        };
        if !task.body.trim().is_empty() {
            let (body, cut) = truncate_chars_flag(task.body.trim(), body_cap);
            let _ = writeln!(out, "{body}");
            if cut {
                let _ = writeln!(
                    out,
                    "(body truncated; run `board show {} --detailed` if needed)",
                    task.id
                );
            }
        }
        let notes_budget = per_item_budget
            .saturating_sub(out.chars().count().saturating_sub(item_start))
            .max(400);
        write_notes_preserving_key_kinds(
            &mut out,
            &task.id,
            &detail.notes,
            &detail.handles,
            MAX_PLAIN_NOTES_BATCH,
            notes_budget,
        );
        out.push('\n');
    }

    if !candidates_section.is_empty() {
        out.push_str(&candidates_section);
        out.push('\n');
    }
    if !setups_section.is_empty() {
        out.push_str(&setups_section);
    }

    out.trim().to_owned()
}

fn format_triage_candidates_section(
    candidates: &[Task],
    batch_ids: &BTreeSet<TaskId>,
    abouts: &BTreeSet<String>,
    budget: usize,
) -> String {
    let mut matching: Vec<&Task> = candidates
        .iter()
        .filter(|t| !batch_ids.contains(&t.id))
        .filter(|t| {
            t.feedback
                .as_ref()
                .is_some_and(|fb| abouts.contains(&fb.about))
        })
        .collect();
    matching.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    matching.truncate(MAX_TRIAGE_CANDIDATES);
    if matching.is_empty() {
        return String::new();
    }
    let mut section =
        format!("### Same-about recent/live candidates (up to {MAX_TRIAGE_CANDIDATES})\n");
    for item in matching {
        let about = item
            .feedback
            .as_ref()
            .map(|f| f.about.as_str())
            .unwrap_or("other");
        let res = item
            .feedback
            .as_ref()
            .and_then(|f| f.resolution.as_ref())
            .map(|r| match &r.task {
                Some(t) => format!("{}->{t}", resolution_str(r.kind)),
                None => resolution_str(r.kind).to_owned(),
            })
            .unwrap_or_else(|| "unresolved".to_owned());
        let line = format!(
            "{} [{}/{}] {}: {}\n",
            item.id,
            status_str(item.status),
            res,
            about,
            one_line(&item.title, 120),
        );
        if section.chars().count() + line.chars().count() > budget {
            break;
        }
        section.push_str(&line);
    }
    section
}

/// Extracts only the `[[setup]]` sections from `setups.toml` whose `id` matches one of
/// `abouts` (exact match, or prefix before `:` such as `skills:rust` matching `skills`).
pub(crate) fn extract_matching_setups(raw_toml: &str, abouts: &BTreeSet<String>) -> String {
    let Ok(doc) = toml::from_str::<toml::Value>(raw_toml) else {
        return String::new();
    };
    let Some(setups) = doc.get("setup").and_then(toml::Value::as_array) else {
        return String::new();
    };

    let mut targets = BTreeSet::new();
    for about in abouts {
        let lower = about.trim().to_ascii_lowercase();
        if !lower.is_empty() {
            if let Some((prefix, _)) = lower.split_once(':') {
                targets.insert(prefix.to_owned());
            }
            targets.insert(lower);
        }
    }

    let mut out = String::new();
    for entry in setups {
        let Some(id) = entry.get("id").and_then(toml::Value::as_str) else {
            continue;
        };
        if !targets.contains(&id.to_ascii_lowercase()) {
            continue;
        }
        let title = entry
            .get("title")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        let channel = entry
            .get("channel")
            .and_then(toml::Value::as_str)
            .unwrap_or("board");
        let owner_board = entry
            .get("owner")
            .and_then(|o| o.get("board"))
            .and_then(toml::Value::as_str);
        let owner_repo = entry
            .get("owner")
            .and_then(|o| o.get("repo"))
            .and_then(toml::Value::as_str);
        let owner_parent = entry
            .get("owner")
            .and_then(|o| o.get("parent"))
            .and_then(toml::Value::as_str);

        let _ = write!(out, "- {id}: {}", one_line(title, 120));
        let _ = write!(out, " | channel={channel}");
        if let Some(b) = owner_board {
            let _ = write!(out, " | board={b}");
        }
        if let Some(r) = owner_repo {
            let _ = write!(out, " | repo={r}");
        }
        if let Some(p) = owner_parent {
            let _ = write!(out, " | parent={p}");
        }
        out.push('\n');
        if let Some(change) = entry.get("change").and_then(toml::Value::as_str) {
            let _ = writeln!(out, "  change: {}", one_line(change, 180));
        }
        if let Some(verify) = entry.get("verify").and_then(toml::Value::as_array) {
            let cmds: Vec<&str> = verify.iter().filter_map(toml::Value::as_str).collect();
            if !cmds.is_empty() {
                let _ = writeln!(out, "  verify: {}", cmds.join("; "));
            }
        }
    }
    out.trim_end().to_owned()
}

/// Pure formatter for verdict pre-packed context (unit-testable without network I/O).
///
/// Includes `board_goal` (when non-empty) and reserves explicit budget for the trailing
/// open/active board task index so duplicate candidates are never truncated off the tail
/// in a multi-proposal batch.
pub(crate) fn format_verdict_context(
    board_goal: &str,
    details: &[TaskDetail],
    board_tasks: &[Task],
) -> String {
    let mut proposal_ids = BTreeSet::new();
    let mut parents = BTreeSet::new();
    let mut keywords = BTreeSet::new();
    for detail in details {
        proposal_ids.insert(detail.task.id.clone());
        if let Some(parent) = &detail.task.parent {
            parents.insert(parent.clone());
        }
        for word in title_keywords(&detail.task.title) {
            keywords.insert(word);
        }
    }

    let goal_section = if !board_goal.trim().is_empty() {
        let (goal, cut) = truncate_chars_flag(board_goal.trim(), VERDICT_GOAL_BUDGET);
        let note = if cut {
            "\n(goal truncated; run `board status --detailed` if needed)"
        } else {
            ""
        };
        format!("### Board goal\n{goal}{note}\n\n")
    } else {
        String::new()
    };

    let index_section = format_verdict_index_section(
        board_tasks,
        &proposal_ids,
        &parents,
        &keywords,
        VERDICT_INDEX_BUDGET,
    );

    let items_budget = SOFT_TOTAL_BUDGET
        .saturating_sub(goal_section.chars().count())
        .saturating_sub(index_section.chars().count());
    let per_item_budget = if details.is_empty() {
        items_budget
    } else {
        (items_budget / details.len()).max(500)
    };

    let mut out = goal_section;
    for detail in details {
        let item_start = out.chars().count();
        let task = &detail.task;
        let proposer = resolve_handle(&detail.handles, task.created_by.as_str());
        let parent_str = task.parent.as_ref().map(TaskId::as_str).unwrap_or("none");
        let _ = writeln!(
            out,
            "### {} [{}] proposer={} parent={} {}",
            task.id,
            priority_str(task.priority),
            proposer,
            parent_str,
            one_line(&task.title, 160),
        );
        let header_used = out.chars().count().saturating_sub(item_start);
        let item_rem = per_item_budget.saturating_sub(header_used);
        let body_cap = (item_rem * 3 / 5).clamp(250, 1_500);
        let acc_cap = (item_rem / 4).clamp(150, 600);
        let mut truncated = false;
        if !task.body.trim().is_empty() {
            let (body, cut) = truncate_chars_flag(task.body.trim(), body_cap);
            truncated |= cut;
            let _ = writeln!(out, "{body}");
        }
        if !task.acceptance.trim().is_empty() {
            let (acc, cut) = truncate_chars_flag(task.acceptance.trim(), acc_cap);
            truncated |= cut;
            let _ = writeln!(out, "Acceptance:\n{acc}");
        }
        let notes_budget = per_item_budget
            .saturating_sub(out.chars().count().saturating_sub(item_start))
            .max(350);
        write_notes_preserving_key_kinds(
            &mut out,
            &task.id,
            &detail.notes,
            &detail.handles,
            MAX_PLAIN_NOTES_BATCH,
            notes_budget,
        );
        if truncated {
            let _ = writeln!(
                out,
                "(body/acceptance truncated; run `board show {} --detailed` if needed)",
                task.id
            );
        }
        out.push('\n');
    }

    if !index_section.is_empty() {
        out.push_str(&index_section);
    }

    out.trim().to_owned()
}

fn format_verdict_index_section(
    board_tasks: &[Task],
    proposal_ids: &BTreeSet<TaskId>,
    parents: &BTreeSet<TaskId>,
    keywords: &BTreeSet<String>,
    budget: usize,
) -> String {
    let mut index: Vec<(u8, usize, &Task)> = board_tasks
        .iter()
        .filter(|t| !proposal_ids.contains(&t.id))
        .filter(|t| {
            matches!(
                t.status,
                TaskStatus::Open | TaskStatus::Doing | TaskStatus::Review | TaskStatus::Blocked
            )
        })
        .map(|t| {
            let same_parent =
                t.parent.as_ref().is_some_and(|p| parents.contains(p)) || parents.contains(&t.id);
            let overlap = title_keywords(&t.title)
                .into_iter()
                .filter(|w| keywords.contains(w))
                .count();
            let tier = if same_parent {
                0
            } else if overlap > 0 {
                1
            } else {
                2
            };
            (tier, overlap, t)
        })
        .collect();

    index.sort_by(|(tier_a, ov_a, a), (tier_b, ov_b, b)| {
        tier_a
            .cmp(tier_b)
            .then_with(|| ov_b.cmp(ov_a))
            .then_with(|| status_rank(a.status).cmp(&status_rank(b.status)))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    index.truncate(MAX_VERDICT_INDEX);

    if index.is_empty() {
        return String::new();
    }
    let mut section = format!(
        "### Open/active board tasks index (up to {MAX_VERDICT_INDEX}, prioritized by parent & title overlap)\n"
    );
    for (_, _, t) in index {
        let parent_part = t
            .parent
            .as_ref()
            .map(|p| format!(" (parent={p})"))
            .unwrap_or_default();
        let line = format!(
            "{} [{} {}]{} {}\n",
            t.id,
            status_str(t.status),
            priority_str(t.priority),
            parent_part,
            one_line(&t.title, 120),
        );
        if section.chars().count() + line.chars().count() > budget {
            break;
        }
        section.push_str(&line);
    }
    section
}

/// Writes task notes in chronological order, ALWAYS preserving every `decision` and `finding`
/// note (unclipped up to `MAX_KEY_NOTE_CHARS`, matching the server's 800-char limit) alongside
/// the most recent plain `note` entries that fit within `budget`. When older plain notes are
/// omitted or any note is clipped, emits an explicit pointer to `board show <id> --detailed`.
fn write_notes_preserving_key_kinds(
    out: &mut String,
    task_id: &TaskId,
    notes: &[Note],
    handles: &BTreeMap<String, String>,
    max_plain_notes: usize,
    budget: usize,
) {
    if notes.is_empty() {
        return;
    }

    let is_key = |k: NoteKind| matches!(k, NoteKind::Decision | NoteKind::Finding);
    let mut keep = vec![false; notes.len()];
    let mut key_count = 0usize;
    for (idx, note) in notes.iter().enumerate() {
        if is_key(note.kind) {
            keep[idx] = true;
            key_count += 1;
        }
    }

    // Also include the newest `max_plain_notes` plain notes (preserving chronological order).
    let mut plain_kept = 0usize;
    for idx in (0..notes.len()).rev() {
        if !keep[idx] && plain_kept < max_plain_notes {
            keep[idx] = true;
            plain_kept += 1;
        }
    }

    // Format key notes first to see how much budget remains for plain notes.
    let mut formatted: Vec<(usize, bool, String)> = Vec::new();
    let mut key_chars = 0usize;
    let mut any_clipped = false;
    for (idx, note) in notes.iter().enumerate() {
        if !keep[idx] {
            continue;
        }
        let key = is_key(note.kind);
        let limit = if key {
            MAX_KEY_NOTE_CHARS
        } else {
            MAX_PLAIN_NOTE_CHARS
        };
        let (text, cut) = truncate_chars_flag(note.text.trim(), limit);
        any_clipped |= cut;
        let by = resolve_handle(handles, note.agent.as_str());
        let date = note.at.format("%Y-%m-%d %H:%MZ");
        let line = format!("- [{date} {} {by}] {text}\n", note_kind_str(note.kind));
        if key {
            key_chars += line.chars().count();
        }
        formatted.push((idx, key, line));
    }

    // If plain notes push past budget, drop the oldest plain notes first while keeping all
    // key (`decision`/`finding`) notes intact.
    let mut plain_budget = budget.saturating_sub(key_chars).saturating_sub(80);
    let mut final_include = vec![false; formatted.len()];
    for (pos, (_, key, _)) in formatted.iter().enumerate() {
        if *key {
            final_include[pos] = true;
        }
    }
    for (pos, (_, key, line)) in formatted.iter().enumerate().rev() {
        if !key {
            let len = line.chars().count();
            if len <= plain_budget {
                final_include[pos] = true;
                plain_budget -= len;
            }
        }
    }

    let shown = final_include.iter().filter(|&&b| b).count();
    let omitted = notes.len().saturating_sub(shown);
    let _ = writeln!(
        out,
        "Notes ({shown} shown of {}, all {key_count} decision/finding included):",
        notes.len()
    );
    for (pos, (_, _, line)) in formatted.into_iter().enumerate() {
        if final_include[pos] {
            out.push_str(&line);
        }
    }
    if omitted > 0 || any_clipped {
        let _ = writeln!(
            out,
            "(truncated: {omitted} older plain note(s) omitted; run `board show {task_id} --detailed` if needed)"
        );
    }
}

fn resolve_handle<'a>(handles: &'a BTreeMap<String, String>, agent: &'a str) -> &'a str {
    handles.get(agent).map(String::as_str).unwrap_or(agent)
}

fn title_keywords(title: &str) -> BTreeSet<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "from", "that", "this", "when", "into", "only", "after",
        "before", "about", "under", "over", "without", "instead",
    ];
    title
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

fn status_rank(status: TaskStatus) -> u8 {
    match status {
        TaskStatus::Doing => 0,
        TaskStatus::Review => 1,
        TaskStatus::Open => 2,
        TaskStatus::Blocked => 3,
        TaskStatus::Done | TaskStatus::Dropped => 4,
    }
}

fn status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Open => "open",
        TaskStatus::Doing => "doing",
        TaskStatus::Review => "review",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        TaskStatus::Dropped => "dropped",
    }
}

fn priority_str(priority: Priority) -> &'static str {
    match priority {
        Priority::P1 => "p1",
        Priority::P2 => "p2",
        Priority::P3 => "p3",
    }
}

fn note_kind_str(kind: NoteKind) -> &'static str {
    match kind {
        NoteKind::Note => "note",
        NoteKind::Finding => "finding",
        NoteKind::Decision => "decision",
        NoteKind::Feedback => "feedback",
    }
}

fn feedback_source_str(source: FeedbackSource) -> &'static str {
    match source {
        FeedbackSource::Agent => "agent",
        FeedbackSource::Capture => "capture",
        FeedbackSource::Signal => "signal",
    }
}

fn feedback_activity_str(activity: FeedbackActivity) -> &'static str {
    match activity {
        FeedbackActivity::Explore => "explore",
        FeedbackActivity::Research => "research",
        FeedbackActivity::Design => "design",
        FeedbackActivity::Plan => "plan",
        FeedbackActivity::Build => "build",
        FeedbackActivity::Review => "review",
        FeedbackActivity::Operate => "operate",
        FeedbackActivity::Other => "other",
    }
}

fn resolution_str(kind: ResolutionKind) -> &'static str {
    match kind {
        ResolutionKind::Routed => "routed",
        ResolutionKind::Duplicate => "duplicate",
        ResolutionKind::Forwarded => "forwarded",
        ResolutionKind::Declined => "declined",
        ResolutionKind::Fixed => "fixed",
        ResolutionKind::Unknown => "unknown",
    }
}

fn one_line(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars_flag(&flat, max_chars).0
}

fn truncate_chars_flag(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    (cut, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use board_proto::{AgentId, ChildTask, Feedback, FeedbackResolution, Timestamp};

    fn ts(raw: &str) -> Timestamp {
        raw.parse().unwrap()
    }

    fn make_task(id: &str, title: &str, status: TaskStatus) -> Task {
        let task_id: TaskId = id.parse().unwrap();
        let board = task_id.board().clone();
        Task {
            id: task_id,
            board,
            parent: None,
            title: title.to_owned(),
            body: String::new(),
            acceptance: String::new(),
            status,
            priority: Priority::P2,
            rank: None,
            blocked_reason: None,
            owner: None,
            owner_stale: false,
            blockers: Vec::new(),
            ready: status == TaskStatus::Open,
            blockers_closed: false,
            proposal: false,
            not_before: None,
            auto: true,
            created_by: "t3:creator".parse::<AgentId>().unwrap(),
            version: 1,
            created_at: ts("2026-11-24T10:00:00Z"),
            updated_at: ts("2026-11-24T10:00:00Z"),
            feedback: None,
            summary: None,
            child_count: 0,
            owner_handle: None,
            snippet: None,
            any_word: false,
            repo: None,
            weight: None,
            resource_units: None,
            requires: Vec::new(),
            tolerations: Vec::new(),
            conflict_keys: Vec::new(),
            affinity: None,
            preferred_runner: None,
            origin: None,
            size: None,
            cooled_at: None,
            ready_at: None,
            report_count: 0,
            effective_priority: None,
        }
    }

    fn make_detail(task: Task, notes: Vec<Note>, children: Vec<ChildTask>) -> TaskDetail {
        TaskDetail {
            task,
            notes,
            evidence: Vec::new(),
            blocks: Vec::new(),
            children: children.iter().map(|c| c.id.clone()).collect(),
            child_tasks: children,
            history: Vec::new(),
            handles: BTreeMap::from([("t3:creator".to_owned(), "t3:crea".to_owned())]),
        }
    }

    #[test]
    fn triage_context_packs_items_same_about_candidates_and_matching_setups() {
        let mut target = make_task(
            "board-feedback-100",
            "Board MCP fails with no agent id",
            TaskStatus::Open,
        );
        target.body = "Detailed report body".to_owned();
        target.feedback = Some(Feedback {
            about: "board".to_owned(),
            activity: Some(FeedbackActivity::Review),
            source: FeedbackSource::Agent,
            signature: "board:no-agent-id".to_owned(),
            reports: 3,
            reporters: vec!["t3:creator".parse().unwrap()],
            last_reported_at: ts("2026-11-24T12:00:00Z"),
            resolution: None,
            recurs: Some("board-feedback-50".parse().unwrap()),
        });

        let mut cand_same = make_task(
            "board-feedback-90",
            "Earlier board MCP identity issue",
            TaskStatus::Done,
        );
        cand_same.updated_at = ts("2026-11-24T11:00:00Z");
        cand_same.feedback = Some(Feedback {
            about: "board".to_owned(),
            activity: None,
            source: FeedbackSource::Agent,
            signature: "board:earlier".to_owned(),
            reports: 5,
            reporters: Vec::new(),
            last_reported_at: ts("2026-11-24T11:00:00Z"),
            resolution: Some(FeedbackResolution {
                kind: ResolutionKind::Routed,
                task: Some("agent-board-694".parse().unwrap()),
                reason: None,
                at: ts("2026-11-24T11:00:00Z"),
                by: "t3:judge".to_owned(),
            }),
            recurs: None,
        });

        let mut cand_other =
            make_task("board-feedback-91", "Unrelated git issue", TaskStatus::Open);
        cand_other.feedback = Some(Feedback {
            about: "git".to_owned(),
            activity: None,
            source: FeedbackSource::Agent,
            signature: "git:other".to_owned(),
            reports: 1,
            reporters: Vec::new(),
            last_reported_at: ts("2026-11-24T11:30:00Z"),
            resolution: None,
            recurs: None,
        });

        let setups = r#"
[[setup]]
id = "board"
title = "Work board"
owner = { repo = "github.com/example/agent-board", board = "agent-board" }
channel = "board"
change = "Open a PR on agent-board"
verify = ["cargo test"]

[[setup]]
id = "git"
title = "Git settings"
owner = { repo = "github.com/example/dotfiles", board = "dotfiles" }
channel = "board"
"#;

        let text = format_triage_context(
            &[make_detail(target, Vec::new(), Vec::new())],
            &[cand_same, cand_other],
            Some(setups),
        );
        assert!(text.contains("board-feedback-100 [p2] (open)"), "{text}");
        assert!(
            text.contains("about: board | source: agent | reports: 3"),
            "{text}"
        );
        assert!(text.contains("recurs: board-feedback-50"), "{text}");
        assert!(
            text.contains("board-feedback-90 [done/routed->agent-board-694] board:"),
            "{text}"
        );
        assert!(!text.contains("board-feedback-91"), "{text}");
        assert!(
            text.contains("- board: Work board | channel=board | board=agent-board"),
            "{text}"
        );
        assert!(!text.contains("- git:"), "{text}");
    }

    #[test]
    fn triage_batch_preserves_all_items_candidates_and_setups_under_budget() {
        let mut details = Vec::new();
        for i in 1..=5 {
            let mut t = make_task(
                &format!("board-feedback-20{i}"),
                &format!("Batch item {i}"),
                TaskStatus::Open,
            );
            t.body = "B".repeat(3_500);
            t.feedback = Some(Feedback {
                about: "board".to_owned(),
                activity: None,
                source: FeedbackSource::Agent,
                signature: format!("board:sig-{i}"),
                reports: 1,
                reporters: Vec::new(),
                last_reported_at: ts("2026-11-24T12:00:00Z"),
                resolution: None,
                recurs: None,
            });
            details.push(make_detail(t, Vec::new(), Vec::new()));
        }
        let mut cand = make_task("board-feedback-199", "Existing duplicate", TaskStatus::Open);
        cand.feedback = Some(Feedback {
            about: "board".to_owned(),
            activity: None,
            source: FeedbackSource::Agent,
            signature: "board:existing".to_owned(),
            reports: 2,
            reporters: Vec::new(),
            last_reported_at: ts("2026-11-24T11:00:00Z"),
            resolution: None,
            recurs: None,
        });
        let setups = r#"
[[setup]]
id = "board"
title = "Work board"
owner = { repo = "github.com/example/agent-board", board = "agent-board" }
channel = "board"
"#;
        let text = format_triage_context(&details, &[cand], Some(setups));
        assert!(
            text.chars().count() < crate::brief::PREPACKED_CONTEXT_CHARS_MAX,
            "5-item batch must fit under PREPACKED_CONTEXT_CHARS_MAX, got {}",
            text.chars().count()
        );
        for i in 1..=5 {
            assert!(text.contains(&format!("board-feedback-20{i}")), "{text}");
        }
        assert!(text.contains("board-feedback-199"), "{text}");
        assert!(text.contains("- board: Work board"), "{text}");
    }

    #[test]
    fn verdict_context_prioritizes_same_parent_and_title_overlap_and_caps_at_30() {
        let mut prop = make_task(
            "factory-10",
            "Add scheduler debounce window for triage",
            TaskStatus::Open,
        );
        prop.parent = Some("factory-1".parse().unwrap());
        prop.body = "Proposal: coalesce rapid triage triggers.".to_owned();
        prop.acceptance = "Tests pass".to_owned();

        let mut same_parent = make_task("factory-2", "Unrelated sibling", TaskStatus::Open);
        same_parent.parent = Some("factory-1".parse().unwrap());

        let mut overlap = make_task(
            "factory-3",
            "Tune triage debounce timing",
            TaskStatus::Doing,
        );
        overlap.updated_at = ts("2026-11-24T09:00:00Z");

        let mut board_tasks = vec![prop.clone(), same_parent, overlap];
        for i in 20..60 {
            board_tasks.push(make_task(
                &format!("factory-{i}"),
                &format!("Generic unrelated task {i}"),
                TaskStatus::Open,
            ));
        }

        let text = format_verdict_context(
            "Keep board latency under 1s and minimize token waste.",
            &[make_detail(prop, Vec::new(), Vec::new())],
            &board_tasks,
        );
        assert!(
            text.contains("### Board goal\nKeep board latency under 1s and minimize token waste."),
            "{text}"
        );
        assert!(
            text.contains("factory-10 [p2] proposer=t3:crea parent=factory-1"),
            "{text}"
        );
        assert!(
            text.contains("Proposal: coalesce rapid triage triggers."),
            "{text}"
        );
        let idx_pos_sibling = text.find("factory-2 [open p2] (parent=factory-1)").unwrap();
        let idx_pos_overlap = text.find("factory-3 [doing p2]").unwrap();
        let idx_pos_generic = text.find("factory-20 [open p2]").unwrap();
        assert!(idx_pos_sibling < idx_pos_overlap && idx_pos_overlap < idx_pos_generic);
        // Excluded target proposal itself from index and capped at 30 rows.
        let index_section = text
            .split("### Open/active board tasks index")
            .nth(1)
            .unwrap();
        let row_count = index_section
            .lines()
            .filter(|l| l.starts_with("factory-"))
            .count();
        assert_eq!(row_count, MAX_VERDICT_INDEX);
        assert!(!index_section.contains("factory-10 "));
    }

    #[test]
    fn review_context_preserves_every_old_decision_and_finding_note_and_children() {
        let mut task = make_task("factory-7", "Implement token telemetry", TaskStatus::Review);
        task.owner = Some("t3:creator".parse().unwrap());
        task.body = "Full implementation details.".to_owned();
        task.acceptance = "cargo test passes\nmetrics emitted".to_owned();

        let mut notes = vec![
            Note {
                id: 1,
                task: task.id.clone(),
                agent: "t3:creator".parse().unwrap(),
                kind: NoteKind::Finding,
                text: "EARLIEST_FINDING: must not be dropped".to_owned(),
                at: ts("2026-11-24T10:00:00Z"),
            },
            Note {
                id: 2,
                task: task.id.clone(),
                agent: "t3:creator".parse().unwrap(),
                kind: NoteKind::Decision,
                text: "review: fail — earliest failure count matters".to_owned(),
                at: ts("2026-11-24T10:05:00Z"),
            },
        ];
        for i in 3..=16 {
            notes.push(Note {
                id: i,
                task: task.id.clone(),
                agent: "t3:creator".parse().unwrap(),
                kind: NoteKind::Note,
                text: format!("Plain progress note #{i}"),
                at: ts("2026-11-24T11:00:00Z"),
            });
        }
        let child = ChildTask {
            id: "factory-8".parse().unwrap(),
            title: "Deploy after merge".to_owned(),
            status: TaskStatus::Open,
            ready: true,
            owner: None,
            owner_stale: false,
        };

        let text = format_review_context(&make_detail(task, notes, vec![child]));
        assert!(
            text.contains("factory-7 [p2] (review) owner=t3:crea"),
            "{text}"
        );
        assert!(
            text.contains("EARLIEST_FINDING: must not be dropped"),
            "{text}"
        );
        assert!(
            text.contains("review: fail — earliest failure count matters"),
            "{text}"
        );
        assert!(text.contains("Plain progress note #16"), "{text}");
        assert!(text.contains("board show factory-7 --detailed"), "{text}");
        assert!(
            text.contains("- factory-8 [open] Deploy after merge"),
            "{text}"
        );
    }

    #[test]
    fn worker_context_packs_body_acceptance_notes_blockers_and_branch() {
        let mut task = make_task("factory-42", "Optimize hook path", TaskStatus::Open);
        task.body = "Remove slow round-trip on PreInvocation.".to_owned();
        task.acceptance = "cargo test --workspace passes".to_owned();
        task.blockers = vec!["factory-40".parse().unwrap()];
        let notes = vec![Note {
            id: 1,
            task: task.id.clone(),
            agent: "t3:creator".parse().unwrap(),
            kind: NoteKind::Finding,
            text: "Must keep halt semantics intact".to_owned(),
            at: ts("2026-11-24T10:00:00Z"),
        }];
        let text = format_worker_context(
            &make_detail(task, notes, Vec::new()),
            Some("task/factory-42"),
            Some("github.com/example/agent-board"),
        );
        assert!(
            text.contains("factory-42 [p2] (open) Optimize hook path"),
            "{text}"
        );
        assert!(
            text.contains("repo: github.com/example/agent-board"),
            "{text}"
        );
        assert!(text.contains("branch: task/factory-42"), "{text}");
        assert!(text.contains("blockers: factory-40"), "{text}");
        assert!(
            text.contains("Remove slow round-trip on PreInvocation."),
            "{text}"
        );
        assert!(text.contains("cargo test --workspace passes"), "{text}");
        assert!(text.contains("Must keep halt semantics intact"), "{text}");
    }

    #[test]
    fn review_context_with_pr_packs_metadata_stat_and_bounds_diff_to_24kb() {
        let mut task = make_task("factory-9", "Add prepacking", TaskStatus::Review);
        // Stress worst-case body + notes + stat + 24 KiB diff so we verify the closing fence
        // and truncation notice always fit inside `REVIEW_PREPACKED_CONTEXT_CHARS_MAX`.
        task.body = "B".repeat(2_500);
        task.acceptance = "A".repeat(1_400);
        let notes: Vec<Note> = (1..=8)
            .map(|i| Note {
                id: i,
                task: task.id.clone(),
                agent: "t3:creator".parse().unwrap(),
                kind: NoteKind::Finding,
                text: "F".repeat(750),
                at: ts("2026-11-24T10:00:00Z"),
            })
            .collect();
        let pr = PullRequest {
            url: "https://github.com/o/r/pull/99".to_owned(),
            state: PrState::Open,
            draft: true,
            base_ref: "main".to_owned(),
            head_ref: "task/factory-9".to_owned(),
            head_oid: "deadbeef1234567890".to_owned(),
            cross_repository: false,
            merge_commit: None,
            merge_state: MergeState::Clean,
            review_decision: ReviewDecision::None,
            checks: Checks::Passed,
            passed_checks: vec!["rust".to_owned(), "web".to_owned()],
        };
        let stat = " src/lib.rs | 10 +++++-----\n".repeat(120);
        let huge_diff = "diff --git a/a.rs b/a.rs\n".to_owned() + &"+🔥line\n".repeat(6_000);
        assert!(huge_diff.len() > REVIEW_DIFF_MAX_BYTES);

        let text = format_review_context_with_pr(
            &make_detail(task.clone(), notes, Vec::new()),
            Some(&pr),
            Some(&stat),
            Some(&huge_diff),
        );
        assert!(
            text.contains("url: https://github.com/o/r/pull/99"),
            "{text}"
        );
        assert!(text.contains("headRefOid: deadbeef1234567890"), "{text}");
        assert!(text.contains("checks: passed (rust, web)"), "{text}");
        assert!(text.contains("#### Diff stat"), "{text}");
        assert!(text.contains("#### Unified diff"), "{text}");
        assert!(
            text.contains("unified diff truncated at 24 KiB"),
            "oversized diff must note truncation: {text}"
        );
        let wrapped = crate::brief::append_prepacked_context_with_limit(
            String::new(),
            &text,
            crate::brief::REVIEW_PREPACKED_CONTEXT_CHARS_MAX,
        );
        assert!(
            wrapped.contains("\n```\n(unified diff truncated at 24 KiB;"),
            "outer wrapper must preserve closing diff fence and truncation notice"
        );
        assert!(
            wrapped.chars().count() <= crate::brief::REVIEW_PREPACKED_CONTEXT_CHARS_MAX,
            "got {} chars",
            wrapped.chars().count()
        );

        // Empty `passed_checks` must NOT report `checks: passed` so reviewers still verify locally.
        let mut pr_no_ci = pr.clone();
        pr_no_ci.passed_checks.clear();
        let no_ci_text = format_review_context_with_pr(
            &make_detail(task, Vec::new(), Vec::new()),
            Some(&pr_no_ci),
            None,
            None,
        );
        assert!(
            no_ci_text.contains("checks: none (no passing CI checks reported)"),
            "{no_ci_text}"
        );
    }

    #[test]
    fn goal_context_packs_goal_counts_recent_done_and_open_tasks() {
        let mut board = Board {
            slug: "factory".parse().unwrap(),
            title: "Factory".to_owned(),
            goal: "Ship zero-waste autopilot runtime.".to_owned(),
            status: board_proto::BoardStatus::Active,
            autopilot: true,
            temporary: false,
            repos: vec!["github.com/o/r".to_owned()],
            home_devices: vec!["mac".parse().unwrap()],
            coordinator: None,
            parallel_tasks: 2,
            parallel_tasks_until: None,
            parallel_tasks_prev: None,
            usage_stop_at: 90,
            usage_stop_at_until: None,
            usage_stop_at_prev: None,
            verdict_judges: true,
            goal_review_every_secs: None,
            kind: board_proto::BoardKind::Project,
            curate_every_secs: None,
            dispatch: board_proto::Dispatch::Daemon,
            counts: board_proto::StatusCounts {
                open: 2,
                doing: 0,
                blocked: 1,
                review: 0,
                done: 12,
                dropped: 1,
                ready: 1,
                ready_auto: Some(0),
            },
            open_asks: 0,
            created_at: ts("2026-11-01T00:00:00Z"),
            updated_at: ts("2026-11-24T12:00:00Z"),
        };
        let mut done_a = make_task("factory-10", "Older done", TaskStatus::Done);
        done_a.updated_at = ts("2026-11-23T10:00:00Z");
        let mut done_b = make_task("factory-11", "Newest done", TaskStatus::Done);
        done_b.updated_at = ts("2026-11-24T12:00:00Z");
        done_b.summary = Some("Merged PR #42".to_owned());
        let open_blocked = make_task("factory-12", "Waiting on user", TaskStatus::Blocked);
        board.counts.blocked = 1;

        let text = format_goal_context(&board, &[done_a, done_b], &[open_blocked]);
        assert!(
            text.contains("### Board goal\nShip zero-waste autopilot runtime."),
            "{text}"
        );
        assert!(
            text.contains("open 2 · doing 0 · blocked 1 · review 0 · done 12 · ready 1 (auto 0)"),
            "{text}"
        );
        let pos_b = text
            .find("factory-11 [p2] Newest done — Merged PR #42")
            .unwrap();
        let pos_a = text.find("factory-10 [p2] Older done").unwrap();
        assert!(pos_b < pos_a, "newest done task must appear first: {text}");
        assert!(
            text.contains("factory-12 [blocked p2] Waiting on user"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn collect_review_git_diff_reads_stat_and_unified_diff_from_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin.git");
        let work = dir.path().join("work");
        let run_git = |cwd: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .expect("git");
            assert!(
                status.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&status.stderr)
            );
            String::from_utf8_lossy(&status.stdout).trim().to_owned()
        };
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        run_git(&origin, &["init", "--bare", "-b", "main"]);
        run_git(&work, &["init", "-b", "main"]);
        run_git(
            &work,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        std::fs::write(work.join("hello.txt"), "one\n").unwrap();
        run_git(&work, &["add", "hello.txt"]);
        run_git(&work, &["commit", "-m", "init"]);
        run_git(&work, &["push", "-u", "origin", "main"]);

        run_git(&work, &["checkout", "-b", "task/factory-9"]);
        std::fs::write(work.join("hello.txt"), "one\ntwo\n").unwrap();
        run_git(&work, &["commit", "-am", "add two"]);
        let head_oid = run_git(&work, &["rev-parse", "HEAD"]);
        run_git(&work, &["push", "-u", "origin", "task/factory-9"]);
        run_git(&work, &["checkout", "main"]);

        let pr = PullRequest {
            url: "https://github.com/o/r/pull/9".to_owned(),
            state: PrState::Open,
            draft: true,
            base_ref: "main".to_owned(),
            head_ref: "task/factory-9".to_owned(),
            head_oid,
            cross_repository: false,
            merge_commit: None,
            merge_state: MergeState::Clean,
            review_decision: ReviewDecision::None,
            checks: Checks::Passed,
            passed_checks: vec!["rust".to_owned()],
        };
        let (stat, diff) =
            collect_review_git_diff(&work, Some(&pr), Some("origin/task/factory-9")).await;
        let stat = stat.expect("diff stat");
        let diff = diff.expect("unified diff");
        assert!(stat.contains("hello.txt"), "{stat}");
        assert!(diff.contains("+two"), "{diff}");
    }
}
