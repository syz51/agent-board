//! The dispatcher's judges, merges and finishes on a daemon-dispatch board
//! (`docs/DESIGN.md`, "Daemon dispatch", Judges, Merging and finishing).
//!
//! Each reconcile reads what the board needs judged, and starts a judge thread for each
//! need while fewer than [`JUDGES_AT_ONCE`] run:
//! - a task in `review` with no standing `review: pass` gets a review judge, in a worktree
//!   on the head of its pull request (or on its base branch when already merged);
//! - pending proposals get a verdict judge per parent task, at most
//!   [`JUDGE_TASKS_AT_ONCE`] proposals each;
//! - ready items on the feedback board get a triage judge;
//! - a board with a goal gets a goal judge once a task was finished and nothing is ready,
//!   in progress, in review or being judged.
//!
//! A home board that is not on autopilot gets the verdict judges alone ([`Judging`]).
//!
//! A task in `review` with a standing pass is closed in code: its pull request is read at
//! most every [`PR_READ_EVERY`], updated when behind, merged once every check passed, and
//! the task is finished with the judge's summary once the pull request is merged (or the
//! task has none) and its worker no longer runs.

use board_proto::{
    AskKind, CreateAsk, DoneTask, EvidenceKind, Judge, JudgeKind, NewEvidence, Note, RegisterJudge,
    SUMMARY_CHARS_MAX, TaskDetail, UpdateTask,
};

use super::*;
use crate::gh::{
    Checks, Gh, MergeState, PrState, PullRequest, ReviewDecision, Updated, pull_request_url,
    pull_request_url_in, repo_of,
};

/// The most judges of one board at work at once; the rest wait, the oldest need first.
pub const JUDGES_AT_ONCE: usize = 2;

/// The most verdict judges at work at once on all the boards off autopilot together
/// ([`Judging::Verdicts`]). Such a board can hold hundreds of proposals that waited while
/// nothing judged them; this keeps that backlog to a part of the device's runs and of the
/// usage, and leaves the rest to the dispatched boards and the user's threads.
pub const HOME_VERDICTS_AT_ONCE: usize = 2;

/// The most proposals one verdict judge, or items one triage judge, is started for.
pub const JUDGE_TASKS_AT_ONCE: usize = 5;

/// How long a task whose judge ended waits for another judge of that kind, unless the task
/// changed since: a judge that ended without its judgment is not started again at once.
pub const JUDGE_RETRY: TimeDelta = TimeDelta::minutes(30);

/// How long a judgment waits after a start that made no thread.
pub const JUDGE_START_RETRY: TimeDelta = TimeDelta::minutes(5);

/// How long a started judge may stay unregistered before the dispatcher gives it up.
pub const JUDGE_REGISTER_WAIT: TimeDelta = TimeDelta::minutes(10);

/// The shortest time between two reads of one task's pull request.
pub const PR_READ_EVERY: TimeDelta = TimeDelta::minutes(2);

/// How long a merge hold (failed checks, a pull request that stays a draft, or GitHub
/// holding the merge in [`MergeState::Waits`] after checks pass) may last on one head
/// commit before the dispatcher voids the pass and reopens the task (or wakes the user
/// when only a human review is missing).
pub const PR_HOLD_WAIT: TimeDelta = TimeDelta::minutes(30);

/// How long the dispatcher waits for a task in `review` without a recorded PR or pushed
/// worker branch on `origin` before falling back to the repository's default base.
pub const REVIEW_PR_WAIT: TimeDelta = TimeDelta::minutes(2);

/// A judge thread the dispatcher started, until its judge ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeRun {
    pub board: BoardSlug,
    pub kind: JudgeKind,
    /// The tasks it judges (for a goal review, the single last finished task when known).
    #[serde(default)]
    pub tasks: Vec<TaskId>,
    /// Its thread or runner-native session id; `None` until `Runner::spawn` returns, as for
    /// a [`Worker`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// Runner kind that owns this judge (`"t3"` when omitted in older state files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// Pre-assigned or resolved board [`AgentId`] when it differs from `<runner>:<thread>`
    /// (for example ACP runs whose `BOARD_AGENT` is pre-assigned before spawning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    /// Daemon-managed worktree record id, when [`WorktreeManager`] created the worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_id: Option<String>,
    /// Path of the daemon-managed worktree, when [`WorktreeManager`] created it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    /// Local run id in `daemon.db`'s `local_runs` table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub title: String,
    pub started_at: Timestamp,
    /// Whether the server has it as a judge of the board.
    #[serde(default)]
    pub registered: bool,
    /// Whether its thread is noted on the task it reviews.
    #[serde(default)]
    pub noted: bool,
    /// Custom worktree sentence for a review judge that did not start on a PR head, kept
    /// so a recovered start sends the same brief.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_note: Option<String>,
    /// True when a [`JudgeKind::Review`] run is an after-merge review judge checking
    /// `after merge:` acceptance checks after the PR merged (or `pr: none`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub after_merge: bool,
}

impl JudgeRun {
    fn same(&self, other: &JudgeRun) -> bool {
        self.board == other.board
            && self.title == other.title
            && self.started_at == other.started_at
    }

    fn names(&self) -> String {
        if self.tasks.is_empty() {
            return self.board.to_string();
        }
        let tasks: Vec<&str> = self.tasks.iter().map(TaskId::as_str).collect();
        tasks.join(", ")
    }
}

/// A task (or, for a goal review, a board) whose judge ended: no judge of that kind starts
/// for it before `until`, unless the task changed since.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeRetry {
    pub until: Timestamp,
    /// The task's last update when the judge ended; `None` when it could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Timestamp>,
}

/// Why a passed pull request has been held from merging on one head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeHoldKind {
    /// One or more checks on the head commit failed.
    FailedChecks,
    /// The pull request stayed a draft after attempting to mark it ready.
    Draft,
    /// Checks passed on a non-draft pull request, and GitHub requires a human review.
    ReviewRequired,
    /// Checks passed on a non-draft pull request, and a reviewer requested changes.
    ChangesRequested,
    /// Checks passed on a non-draft pull request, and GitHub held the merge for another reason.
    MergeWaits,
}

/// A timed merge hold on one head commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeHold {
    pub kind: MergeHoldKind,
    pub head_oid: String,
    pub since: Timestamp,
}

/// What the dispatcher knows of the pull request of a task it closes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Merge {
    /// When it last read the pull request.
    pub read_at: Timestamp,
    /// The head commit whose failed checks it noted on the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_head: Option<String>,
    /// The current merge hold on one head commit, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<MergeHold>,
}

impl Merge {
    pub(crate) fn new(read_at: Timestamp) -> Self {
        Self {
            read_at,
            failed_head: None,
            hold: None,
        }
    }

    /// Clears hold and failure tracking carried over from an earlier review round before
    /// `parked_at`.
    pub(crate) fn reset_if_before(&mut self, parked_at: Timestamp) {
        if self.read_at < parked_at {
            self.failed_head = None;
            self.hold = None;
        } else if self.hold.as_ref().is_some_and(|h| h.since < parked_at) {
            self.hold = None;
        }
    }

    /// Records or advances a timed hold of `kind` on `head_oid` at `now`.
    ///
    /// Returns `true` once the hold has lasted at least [`PR_HOLD_WAIT`].
    pub(crate) fn track_hold(
        &mut self,
        kind: MergeHoldKind,
        head_oid: &str,
        now: Timestamp,
    ) -> bool {
        let same = self
            .hold
            .as_ref()
            .is_some_and(|h| h.kind == kind && h.head_oid == head_oid);
        if !same {
            self.hold = Some(MergeHold {
                kind,
                head_oid: head_oid.to_owned(),
                since: now,
            });
            return false;
        }
        let hold = self.hold.as_mut().expect("checked above");
        if now < hold.since {
            hold.since = now;
            return false;
        }
        now - hold.since >= PR_HOLD_WAIT
    }

    pub(crate) fn clear_hold(&mut self) {
        self.hold = None;
    }
}

/// Outcome of handling an answered `dispatch:*` human-review ask.
pub(crate) enum DispatchAskOutcome {
    /// Task was moved back from `blocked` to `review` with its standing pass intact.
    ResumedReview,
    /// Pass was voided and the task was moved from `blocked` to `open`.
    Reopened,
    /// Task was no longer blocked on this ask (or has another open ask).
    Noop(String),
}

/// Whether `event` ends an earlier `review: pass`: a move to `doing` or `open`, or any move
/// to `review` other than a `dispatch:*` daemon returning a task from `blocked` after a
/// human-review ask was answered.
pub(crate) fn invalidates_review_pass(event: &board_proto::Event) -> bool {
    if event.event_type != EventType::TaskUpdated {
        return false;
    }
    match event.payload["to"].as_str() {
        Some("doing" | "open") => true,
        Some("review") => {
            !(event.payload["from"] == "blocked" && event.actor.starts_with("dispatch:"))
        }
        _ => false,
    }
}

/// Outcome of advancing one passed pull request toward its merge.
pub(crate) enum AdvanceOutcome {
    /// The pull request is merged.
    Merged(PullRequest),
    /// The pass was voided and the task was set `open`.
    Reopened(String),
    /// The dispatcher created (or reused) an action ask for a required human review and moved
    /// the task to `blocked`.
    BlockedForReview { ask_id: i64, reason: String },
    /// The pull request is still waiting.
    Deferred(String),
}

/// Whether the user's answer to a dispatcher review ask requests reopening the task for a worker.
pub(crate) fn answer_requests_reopen(answer: &str) -> bool {
    answer.trim().to_ascii_lowercase().starts_with("reopen")
}

/// Judge runs read one by one; those this version cannot read are skipped.
pub(super) fn lenient_judges<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<JudgeRun>, D::Error> {
    let serde_json::Value::Array(runs) = serde_json::Value::deserialize(d)? else {
        return Ok(Vec::new());
    };
    Ok(runs
        .into_iter()
        .filter_map(|run| match serde_json::from_value(run) {
            Ok(run) => Some(run),
            Err(_) => {
                tracing::warn!("watch state: skipped a judge this version cannot read");
                None
            }
        })
        .collect())
}

/// Each judge retry read on its own, like the wake-ups.
pub(super) fn lenient_judge_retry<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, JudgeRetry>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the judge retry",
    ))
}

/// Each task's merge read on its own, like the wake-ups.
pub(super) fn lenient_merges<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<TaskId, Merge>, D::Error> {
    Ok(lenient_by_key(
        serde_json::Value::deserialize(d)?,
        "the merge",
    ))
}

/// What a reconcile does for a board's judgments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Judging {
    /// Every judge the board needs, and its reviewed tasks closed: a board this device
    /// dispatches.
    All,
    /// Verdict judges for the pending proposals, and nothing else: an active home board
    /// that is not on autopilot (the user's choice, ask #171 of agent-board-491).
    Verdicts,
}

/// Whether a review judge is running the initial pre-merge review or the post-merge
/// check pass after the PR merged (or `pr: none`).
#[derive(Debug, Clone)]
enum ReviewMode {
    PreMerge {
        /// Whether the current review attempt's notes explicitly declare `pr: none`.
        no_code: bool,
        /// The branch the dispatcher gave the worker (`task/<id>` or its time-suffixed
        /// form), tried on `origin` when no PR is recorded yet.
        worker_branch: Option<String>,
        /// When the task last moved to `review`.
        parked_at: Timestamp,
    },
    AfterMerge {
        /// Base branch recorded in the dispatcher's merged handoff note, if any.
        base_ref: Option<String>,
        /// Merge commit SHA recorded in the dispatcher's merged handoff note, if any.
        merge_commit: Option<String>,
    },
}

/// Extra context needed to choose the worktree base and brief for a review judge.
#[derive(Debug, Clone)]
struct ReviewWant {
    /// The pull request URL recorded on the task, if any.
    pr: Option<String>,
    mode: ReviewMode,
}

/// Information returned by `close_reviewed` when a merged (or `pr: none`) task still has
/// `after merge:` checks that require an after-merge review judge.
#[derive(Debug, Clone)]
struct AfterMergeHandoff {
    pr: Option<String>,
    base_ref: Option<String>,
    merge_commit: Option<String>,
}

/// Whether `start_judge` actually launched a judge thread or deferred it for a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartOutcome {
    Started,
    Waited,
}

/// A judgment the board needs.
#[derive(Debug, Clone)]
struct Want {
    kind: JudgeKind,
    tasks: Vec<TaskId>,
    title: String,
    /// Since when it has waited: the oldest goes first.
    since: Timestamp,
    /// Present for [`JudgeKind::Review`].
    review: Option<ReviewWant>,
}

/// The judges at work on a board.
struct AtWork {
    count: usize,
    tasks: Vec<(JudgeKind, TaskId)>,
}

impl AtWork {
    /// The server's running judges, and the runs it does not know yet.
    fn of(listed: &[Judge], runs: &[JudgeRun], slug: &BoardSlug) -> Self {
        let registered = listed.iter().filter(|j| j.running);
        let starting = runs.iter().filter(|r| r.board == *slug && !r.registered);
        let mut tasks: Vec<(JudgeKind, TaskId)> = registered
            .clone()
            .flat_map(|j| j.tasks.iter().map(|t| (j.kind, t.clone())))
            .collect();
        tasks.extend(
            starting
                .clone()
                .flat_map(|r| r.tasks.iter().map(|t| (r.kind, t.clone()))),
        );
        Self {
            count: registered.count() + starting.count(),
            tasks,
        }
    }

    fn judges(&self, kind: JudgeKind, task: &TaskId) -> bool {
        self.tasks.iter().any(|(k, t)| *k == kind && t == task)
    }
}

/// What the review notes of a task say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewVerdict {
    Pass,
    Fail,
    /// The dispatcher withdrew a pass: the pull request could not be merged.
    Void,
}

/// The verdict a note records: its text starts `review: pass`, `review: fail` or
/// `review: void`, after the note's kind when the author wrote it first.
pub(crate) fn review_verdict(text: &str) -> Option<ReviewVerdict> {
    let rest = after_review(text)?.to_lowercase();
    if rest.starts_with("pass") {
        Some(ReviewVerdict::Pass)
    } else if rest.starts_with("fail") {
        Some(ReviewVerdict::Fail)
    } else if rest.starts_with("void") {
        Some(ReviewVerdict::Void)
    } else {
        None
    }
}

/// The text after `review:` at the start of a note.
fn after_review(text: &str) -> Option<&str> {
    let mut text = text.trim_start();
    for kind in ["decision:", "finding:", "note:"] {
        if let Some(rest) = text.strip_prefix(kind) {
            text = rest.trim_start();
            break;
        }
    }
    Some(text.strip_prefix("review:")?.trim_start())
}

/// Where the review of a task in `review` stands.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Reviewed<'a> {
    /// A judge passed it, and nothing changed since: the dispatcher merges (if needed)
    /// and finishes it, or hands off to an after-merge review judge if `after merge:`
    /// checks are listed.
    Passed(&'a Note),
    /// A judge passed it with `after merge:` checks and the dispatcher already recorded
    /// the merge (`AFTER_MERGE_READY_NOTE`): an after-merge review judge runs next.
    NeedsAfterMerge(&'a Note),
    /// The dispatcher withdrew its pass and has not set it `open` yet.
    Withdrawn,
    /// It waits for a review judge.
    Waits,
}

/// The start of the note the dispatcher adds for the review judge it started.
pub(crate) const JUDGE_NOTE: &str = "note: the dispatcher started a review judge for this task:";

/// The start of the Trail Rule note the dispatcher adds once a passed task's PR merged
/// (or had `pr: none`) and its `review: pass` note listed `after merge:` checks.
pub(crate) const AFTER_MERGE_READY_NOTE: &str = "note: merged; waiting for after-merge checks";

/// Structured metadata parsed from an [`AFTER_MERGE_READY_NOTE`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AfterMergeReadyInfo {
    pub pr_url: Option<String>,
    pub merge_commit: Option<String>,
    pub base_ref: Option<String>,
}

/// The standing review of `detail`'s task: its newest `review:` decision note. A pass
/// stands only when a review judge that `dispatcher` started for the task wrote it, as
/// the dispatcher's own note on the task names it, and the task has not moved to `review`
/// again since.
fn reviewed<'a>(detail: &'a TaskDetail, dispatcher: &AgentId) -> Reviewed<'a> {
    let newest = detail.notes.iter().rev().find_map(|n| {
        (n.kind == NoteKind::Decision)
            .then(|| review_verdict(&n.text))
            .flatten()
            .map(|verdict| (n, verdict))
    });
    let Some((note, verdict)) = newest else {
        return Reviewed::Waits;
    };
    let parked_again = detail
        .history
        .iter()
        .any(|e| invalidates_review_pass(e) && e.at > note.at);
    if parked_again {
        return Reviewed::Waits;
    }
    let judge = format!("(agent {})", note.agent);
    let by_judge = detail.task.owner.as_ref() != Some(&note.agent)
        && detail.notes.iter().any(|n| {
            n.agent == *dispatcher && n.text.starts_with(JUDGE_NOTE) && n.text.ends_with(&judge)
        });
    match verdict {
        ReviewVerdict::Pass if by_judge => {
            if !after_merge_checks(&note.text).is_empty()
                && has_after_merge_ready_note_after(detail, note.id)
            {
                Reviewed::NeedsAfterMerge(note)
            } else {
                Reviewed::Passed(note)
            }
        }
        ReviewVerdict::Void => Reviewed::Withdrawn,
        ReviewVerdict::Pass | ReviewVerdict::Fail => Reviewed::Waits,
    }
}

/// A `<name>: <value>` line of a pass note, as `prompts/judges/review.md` asks for them.
fn tagged<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let line = line.trim().trim_start_matches(['-', '*']).trim_start();
    Some(line.strip_prefix(name)?.strip_prefix(':')?.trim())
}

/// An `after merge: <check>` (or `after-merge: <check>`) tag line, case-insensitive on the
/// tag name.
fn tagged_after_merge(line: &str) -> Option<&str> {
    let line = line.trim().trim_start_matches(['-', '*']).trim_start();
    let (tag, val) = line.split_once(':')?;
    let tag = tag.trim();
    if !tag.eq_ignore_ascii_case("after merge") && !tag.eq_ignore_ascii_case("after-merge") {
        return None;
    }
    Some(val.trim())
}

/// The post-merge acceptance checks a pass note left for after the merge, one
/// `after merge: <check>` (or `after-merge: <check>`) line each. Blank, `none`, and `n/a`
/// values are ignored.
pub(crate) fn after_merge_checks(pass: &str) -> Vec<String> {
    pass.lines()
        .filter_map(|line| {
            let val = tagged_after_merge(line)?;
            let norm = val.trim_end_matches(['.', ',', ';']);
            if norm.is_empty()
                || norm.eq_ignore_ascii_case("none")
                || norm.eq_ignore_ascii_case("n/a")
            {
                None
            } else {
                Some(val.to_owned())
            }
        })
        .collect()
}

/// Formats the Trail Rule note written once a passed task's PR merged (or had `pr: none`)
/// and its `review: pass` note listed `after merge:` checks.
pub(crate) fn format_after_merge_ready_note(
    pass_id: i64,
    merged: Option<&PullRequest>,
    left: &[String],
) -> String {
    let mut lines = vec![format!("{AFTER_MERGE_READY_NOTE} (pass #{pass_id}):")];
    match merged {
        Some(pr) => {
            lines.push(format!("pr: {}", pr.url));
            if let Some(commit) = pr.merge_commit.as_deref().filter(|c| !c.is_empty()) {
                lines.push(format!("commit: {commit}"));
            }
            if !pr.base_ref.is_empty() {
                lines.push(format!("base: {}", pr.base_ref));
            }
        }
        None => {
            lines.push("pr: none".to_owned());
        }
    }
    for check in left {
        lines.push(format!("left: {check}"));
    }
    clip_note(lines.join("\n"))
}

/// Parses PR URL, merge commit, and base branch from an [`AFTER_MERGE_READY_NOTE`] body.
fn parse_after_merge_ready_note(text: &str) -> AfterMergeReadyInfo {
    let pr_url = text.lines().find_map(|l| tagged(l, "pr")).and_then(|v| {
        if v.trim_end_matches('.').eq_ignore_ascii_case("none") {
            None
        } else {
            pull_request_url(v)
        }
    });
    let merge_commit = text
        .lines()
        .find_map(|l| tagged(l, "commit"))
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned);
    let base_ref = text
        .lines()
        .find_map(|l| tagged(l, "base"))
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned);
    AfterMergeReadyInfo {
        pr_url,
        merge_commit,
        base_ref,
    }
}

/// Whether a dispatcher (`dispatch:*`) has written [`AFTER_MERGE_READY_NOTE`] on `detail`
/// after `pass_id`.
pub(crate) fn has_after_merge_ready_note_after(detail: &TaskDetail, pass_id: i64) -> bool {
    detail.notes.iter().any(|n| {
        n.id > pass_id
            && n.agent.source() == AgentSource::Dispatch
            && n.text.starts_with(AFTER_MERGE_READY_NOTE)
    })
}

/// Returns metadata from the newest [`AFTER_MERGE_READY_NOTE`] written by a dispatcher
/// after `pass_id`, if any.
pub(crate) fn after_merge_ready_info(
    detail: &TaskDetail,
    pass_id: i64,
) -> Option<AfterMergeReadyInfo> {
    detail
        .notes
        .iter()
        .rev()
        .find(|n| {
            n.id > pass_id
                && n.agent.source() == AgentSource::Dispatch
                && n.text.starts_with(AFTER_MERGE_READY_NOTE)
        })
        .map(|n| parse_after_merge_ready_note(&n.text))
}

/// Collects all [`AFTER_MERGE_READY_NOTE`] notes written by a dispatcher in the current
/// review round strictly before `pass_id` (newest first, stopping if the task re-entered
/// `review` or had a `review: fail` / `review: void` decision in between).
fn prior_after_merge_ready_notes(detail: &TaskDetail, pass_id: i64) -> Vec<&Note> {
    let mut found = Vec::new();
    for note in detail.notes.iter().rev() {
        if note.id >= pass_id {
            continue;
        }
        let parked_after = detail
            .history
            .iter()
            .any(|e| invalidates_review_pass(e) && e.at > note.at);
        if parked_after {
            break;
        }
        if note.kind == NoteKind::Decision
            && matches!(
                review_verdict(&note.text),
                Some(ReviewVerdict::Fail | ReviewVerdict::Void)
            )
        {
            break;
        }
        if note.agent.source() == AgentSource::Dispatch
            && note.text.starts_with(AFTER_MERGE_READY_NOTE)
        {
            found.push(note);
        }
    }
    found
}

/// Finds the newest [`AFTER_MERGE_READY_NOTE`] written in the same review round strictly
/// before `pass_id`.
fn prior_after_merge_ready_note(detail: &TaskDetail, pass_id: i64) -> Option<&Note> {
    prior_after_merge_ready_notes(detail, pass_id)
        .into_iter()
        .next()
}

/// If an [`AFTER_MERGE_READY_NOTE`] already ran earlier in the current review round before
/// `pass_id`, returns `Some(pr_url_or_none)` as the authoritative PR recorded for that
/// handoff.
pub(crate) fn prior_after_merge_ready_pr(
    detail: &TaskDetail,
    pass_id: i64,
) -> Option<Option<String>> {
    prior_after_merge_ready_note(detail, pass_id)
        .map(|n| parse_after_merge_ready_note(&n.text).pr_url)
}

/// Whether handing off `pass_id` (which still lists `after merge:` checks) would repeat an
/// after-merge handoff in the same review round: true when an [`AFTER_MERGE_READY_NOTE`]
/// already ran in this round and either `current_pr` is `None`, matches any previously
/// handed-off PR in this round, or two handoffs already ran in this round.
pub(crate) fn after_merge_loop_detected(
    detail: &TaskDetail,
    pass_id: i64,
    current_pr: Option<&str>,
) -> bool {
    let prior = prior_after_merge_ready_notes(detail, pass_id);
    if prior.is_empty() {
        return false;
    }
    let Some(url) = current_pr else {
        return true;
    };
    if prior.len() >= 2 {
        return true;
    }
    prior
        .iter()
        .any(|n| parse_after_merge_ready_note(&n.text).pr_url.as_deref() == Some(url))
}

/// Returns the newest `review: pass` decision note in the current review round that lists
/// `after merge:` checks and is already followed by a dispatcher's
/// [`AFTER_MERGE_READY_NOTE`]. Used as a fallback in pull dispatch when the server
/// authorized a legacy review judge via `judges`/`board_updated` without a `JUDGE_NOTE`.
pub(crate) fn pending_after_merge_pass(detail: &TaskDetail) -> Option<&Note> {
    let newest = detail.notes.iter().rev().find_map(|n| {
        (n.kind == NoteKind::Decision)
            .then(|| review_verdict(&n.text))
            .flatten()
            .map(|verdict| (n, verdict))
    })?;
    let (note, verdict) = newest;
    if verdict != ReviewVerdict::Pass {
        return None;
    }
    let parked_again = detail
        .history
        .iter()
        .any(|e| invalidates_review_pass(e) && e.at > note.at);
    if parked_again || detail.task.owner.as_ref() == Some(&note.agent) {
        return None;
    }
    (!after_merge_checks(&note.text).is_empty()
        && has_after_merge_ready_note_after(detail, note.id))
    .then_some(note)
}

/// Collects check evidence for a finishing `pass` note: when `pass` concludes an
/// after-merge review round (preceded by an [`AFTER_MERGE_READY_NOTE`] in the same review
/// round), includes both the pre-merge pass's checks and `pass`'s checks, deduplicated in
/// order.
pub(crate) fn collect_pass_checks(detail: &TaskDetail, pass: &Note) -> Vec<NewEvidence> {
    let mut checks = Vec::new();
    if let Some(ready) = prior_after_merge_ready_note(detail, pass.id) {
        let pre_pass = detail.notes.iter().rev().find(|n| {
            n.id < ready.id
                && n.kind == NoteKind::Decision
                && review_verdict(&n.text) == Some(ReviewVerdict::Pass)
                && !detail
                    .history
                    .iter()
                    .any(|e| invalidates_review_pass(e) && e.at > n.at)
        });
        if let Some(pre) = pre_pass {
            for check in passed_checks(&pre.text) {
                if !checks
                    .iter()
                    .any(|c: &NewEvidence| c.value == check.value && c.exit_code == check.exit_code)
                {
                    checks.push(check);
                }
            }
        }
    }
    for check in passed_checks(&pass.text) {
        if !checks
            .iter()
            .any(|c: &NewEvidence| c.value == check.value && c.exit_code == check.exit_code)
        {
            checks.push(check);
        }
    }
    checks
}

/// The pull request a review passed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PassedPr {
    Url(String),
    /// The task has none.
    None,
    /// The notes name one the dispatcher must not merge, or cannot read: why.
    Unclear(String),
}

/// The pull request the review passed: the pass note's `pr:` line, else the last pull
/// request it links, else the one in the task owner's newest note before it. `pr: none`
/// is none. It must be in one of the board's `repos`, when the board lists any.
pub(crate) fn passed_pr(detail: &TaskDetail, pass: &Note, repos: &[String]) -> PassedPr {
    let named = pass.text.lines().find_map(|l| tagged(l, "pr"));
    if named.is_some_and(|n| n.trim_end_matches('.').eq_ignore_ascii_case("none")) {
        return PassedPr::None;
    }
    let url = named
        .and_then(pull_request_url)
        .or_else(|| pull_request_url(&pass.text))
        .or_else(|| {
            // When this pass concludes an after-merge review round, the dispatcher's
            // `AFTER_MERGE_READY_NOTE` authoritatively recorded the PR (or `pr: none`), so
            // consult it before older task-owner notes that might cite an unrelated PR.
            if let Some(recorded) = prior_after_merge_ready_pr(detail, pass.id) {
                return recorded;
            }
            detail
                .notes
                .iter()
                .rev()
                .filter(|n| n.id < pass.id && Some(&n.agent) == detail.task.owner.as_ref())
                .find_map(|n| pull_request_url(&n.text))
        });
    match (url, named) {
        (Some(url), _) => {
            let known = repos.is_empty()
                || repo_of(&url).is_some_and(|r| repos.iter().any(|k| k.eq_ignore_ascii_case(&r)));
            if known {
                PassedPr::Url(url)
            } else {
                PassedPr::Unclear(format!("{url} is in none of the board's repositories"))
            }
        }
        (None, Some(named)) => PassedPr::Unclear(format!(
            "its pass note names the pull request `{named}`, not by its URL"
        )),
        (None, None) => PassedPr::None,
    }
}

/// The pull request the newest note of a task names, in one of the board's `repos` when
/// the board lists any.
pub(crate) fn noted_pr(detail: &TaskDetail, repos: &[String]) -> Option<String> {
    detail
        .notes
        .iter()
        .rev()
        .find_map(|n| pull_request_url_in(&n.text, repos))
}

/// Whether `text` has a `pr: none` tag line (e.g. `pr: none`, `- pr: none`, `* pr: none.`),
/// matching [`passed_pr`]'s line format so prose merely discussing `pr: none` does not match.
fn has_pr_none(text: &str) -> bool {
    text.lines().any(|line| {
        tagged(line, "pr").is_some_and(|v| {
            v.trim_end_matches(['.', ',', ';'])
                .eq_ignore_ascii_case("none")
        })
    })
}

/// Whether the current review attempt's notes explicitly declare `pr: none` on a `pr:` line.
/// Scans backward until the newest dispatcher worker-start note, any prior `review:`
/// decision note (`pass`, `fail` or `void`), or a newer note naming a same-repo PR URL
/// without `pr: none`, so an older round's `pr: none` never suppresses a later code attempt
/// while a current `pr: none` overrides earlier or related PR citations.
pub(crate) fn noted_pr_none(detail: &TaskDetail, repos: &[String]) -> bool {
    for note in detail.notes.iter().rev() {
        if note.agent.source() == AgentSource::Dispatch {
            if note.text.starts_with(WORKER_NOTE_PREFIX) {
                break;
            }
            continue;
        }
        if note.kind == NoteKind::Decision && review_verdict(&note.text).is_some() {
            break;
        }
        if has_pr_none(&note.text) {
            return true;
        }
        if pull_request_url_in(&note.text, repos).is_some() {
            return false;
        }
    }
    false
}

/// The branch the dispatcher last gave a worker for `detail`'s task, from its worker-start
/// note. Returns `Some(None)` when the newest dispatcher start note ran in-place without a
/// branch, and `None` when no dispatcher start note exists.
pub(crate) fn noted_worker_branch(detail: &TaskDetail) -> Option<Option<String>> {
    for note in detail.notes.iter().rev() {
        if note.agent.source() != AgentSource::Dispatch {
            continue;
        }
        if let Some(rest) = note.text.strip_prefix(WORKER_NOTE_BRANCH) {
            let branch = rest.split_once(": ").map(|(b, _)| b.trim())?;
            if crate::worktree::valid_branch_name(branch) {
                return Some(Some(branch.to_owned()));
            }
            return Some(None);
        }
        if note.text.starts_with(WORKER_NOTE_IN_PLACE) {
            return Some(None);
        }
    }
    None
}

/// When `detail`'s task last moved into `review`: its newest `task_updated` event moving to
/// `review`, else `task.updated_at`.
pub(crate) fn parked_in_review_at(detail: &TaskDetail) -> Timestamp {
    detail
        .history
        .iter()
        .rev()
        .find(|e| e.event_type == EventType::TaskUpdated && e.payload["to"] == "review")
        .map_or(detail.task.updated_at, |e| e.at)
}

/// Whether `now` is still inside the bounded [`REVIEW_PR_WAIT`] window after `parked_at`.
/// Allows up to 5 minutes of forward server-clock skew without starving wildly future times.
pub(crate) fn within_pr_wait(parked_at: Timestamp, now: Timestamp) -> bool {
    let elapsed = now - parked_at;
    elapsed >= -TimeDelta::minutes(5) && elapsed < REVIEW_PR_WAIT
}

/// The checks a pass note records, one `check: <command> => <exit code>` line each.
pub(crate) fn passed_checks(pass: &str) -> Vec<NewEvidence> {
    pass.lines()
        .filter_map(|line| {
            let (command, code) = tagged(line, "check")?.rsplit_once("=>")?;
            let code = code.trim().trim_start_matches("exit").trim();
            Some(NewEvidence {
                kind: EvidenceKind::Check,
                value: command.trim().trim_matches('`').to_owned(),
                exit_code: Some(code.parse().ok()?),
            })
        })
        .filter(|check| !check.value.is_empty())
        .collect()
}

/// The judge's summary for the user: its pass note without the verdict, the check lines
/// and the pull request line.
pub(crate) fn passed_summary(pass: &str) -> String {
    let body = after_review(pass).unwrap_or(pass);
    let body = if body.to_lowercase().starts_with("pass") {
        &body["pass".len()..]
    } else {
        body
    };
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| {
            tagged(l, "check").is_none()
                && tagged(l, "pr").is_none()
                && tagged_after_merge(l).is_none()
        })
        .collect();
    let text = lines.join("\n");
    let text = text
        .trim()
        .trim_start_matches(['.', ':', ',', ';', '-', '—'])
        .trim();
    if text.is_empty() {
        return "The review passed.".to_owned();
    }
    if text.chars().count() <= SUMMARY_CHARS_MAX {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(SUMMARY_CHARS_MAX - 1).collect();
    cut.push('…');
    cut
}

fn retry_key(kind: JudgeKind, task: &TaskId) -> String {
    format!("{}:{task}", brief_kind(kind).as_str())
}

fn after_merge_retry_key(task: &TaskId) -> String {
    format!("after-merge:{task}")
}

fn goal_key(slug: &BoardSlug) -> String {
    format!("goal:{slug}")
}

fn brief_kind(kind: JudgeKind) -> crate::brief::JudgeKind {
    match kind {
        JudgeKind::Review => crate::brief::JudgeKind::Review,
        JudgeKind::Verdict => crate::brief::JudgeKind::Verdict,
        JudgeKind::Goal => crate::brief::JudgeKind::Goal,
        JudgeKind::Triage => crate::brief::JudgeKind::Triage,
    }
}

impl<T: T3 + 'static> Daemon<T> {
    /// Follows the judges started on `board`, closes its reviewed tasks, and starts the
    /// judges it needs. `in_progress` is how many tasks hold a slot. With
    /// [`Judging::Verdicts`] it follows the judges, starts verdict judges, and reads and
    /// changes nothing else of the board.
    pub(super) async fn judge_and_close(
        &mut self,
        board: &Board,
        agent: &AgentId,
        judging: Judging,
        in_progress: usize,
        now: Timestamp,
    ) -> Result<()> {
        let slug = &board.slug;
        let all = judging == Judging::All;
        let (repo_key, project) = self.host_project(board)?;
        let client = self.client().with_agent(Some(agent.clone()));
        self.settle_judge_starts(board, &client, &project, now)
            .await;
        let listed = client
            .judges(slug)
            .await
            .context("listing the board's judges")?;
        self.end_judges(slug, &client, &listed, now).await;
        // Off autopilot no lease gives the board to one device: the home device whose
        // verdict judge is at work has its proposals, and the server refuses another's.
        if !all && listed.iter().any(|j| j.registered_by != *agent) {
            return self.save();
        }
        self.state.judge_retry.retain(|_, retry| retry.until > now);
        let at_work = AtWork::of(&listed, &self.state.judges, slug);
        let mut wants = Vec::new();

        let in_review: Vec<Task> = if all {
            client
                .tasks(&TaskQuery {
                    board: Some(slug.clone()),
                    status: Some(TaskStatus::Review),
                    limit: Some(board_proto::TASKS_LIMIT_MAX),
                    ..TaskQuery::default()
                })
                .await
                .context("listing the tasks in review")?
                .into_iter()
                .filter(|t| t.status == TaskStatus::Review)
                .collect()
        } else {
            Vec::new()
        };
        self.state
            .merges
            .retain(|task, _| task.board() != *slug || in_review.iter().any(|t| t.id == *task));
        for task in &in_review {
            if at_work.judges(JudgeKind::Review, &task.id) {
                continue;
            }
            let detail = match client.task(&task.id).await {
                Ok(detail) => detail,
                Err(err) => {
                    tracing::warn!("reading {} in review: {err:#}", task.id);
                    continue;
                }
            };
            match reviewed(&detail, agent) {
                Reviewed::Passed(pass) => {
                    match self
                        .close_reviewed(board, agent, &client, &project, &detail, pass, now)
                        .await
                    {
                        Ok(Some(handoff)) => {
                            if !self.judge_waits(&after_merge_retry_key(&task.id), task) {
                                wants.push(Want {
                                    kind: JudgeKind::Review,
                                    tasks: vec![task.id.clone()],
                                    title: format!("after-merge {} {}", task.id, task.title),
                                    since: task.updated_at,
                                    review: Some(ReviewWant {
                                        pr: handoff.pr,
                                        mode: ReviewMode::AfterMerge {
                                            base_ref: handoff.base_ref,
                                            merge_commit: handoff.merge_commit,
                                        },
                                    }),
                                });
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::warn!("closing {}: {err:#}", task.id);
                        }
                    }
                }
                Reviewed::NeedsAfterMerge(pass) => {
                    if self.judge_waits(&after_merge_retry_key(&task.id), task) {
                        continue;
                    }
                    let info = after_merge_ready_info(&detail, pass.id);
                    let pr =
                        info.as_ref().and_then(|i| i.pr_url.clone()).or_else(|| {
                            match passed_pr(&detail, pass, &board.repos) {
                                PassedPr::Url(url) => Some(url),
                                _ => None,
                            }
                        });
                    wants.push(Want {
                        kind: JudgeKind::Review,
                        tasks: vec![task.id.clone()],
                        title: format!("after-merge {} {}", task.id, task.title),
                        since: task.updated_at,
                        review: Some(ReviewWant {
                            pr,
                            mode: ReviewMode::AfterMerge {
                                base_ref: info.as_ref().and_then(|i| i.base_ref.clone()),
                                merge_commit: info.as_ref().and_then(|i| i.merge_commit.clone()),
                            },
                        }),
                    });
                }
                Reviewed::Withdrawn => {
                    if let Err(err) = client.task_action(&task.id, "release").await {
                        tracing::warn!("setting {} open: {err:#}", task.id);
                    }
                }
                Reviewed::Waits => {
                    if self.judge_waits(&retry_key(JudgeKind::Review, &task.id), task) {
                        continue;
                    }
                    let no_code = noted_pr_none(&detail, &board.repos);
                    let pr = if no_code {
                        None
                    } else {
                        noted_pr(&detail, &board.repos)
                    };
                    let worker_branch = if pr.is_none() && !no_code {
                        match noted_worker_branch(&detail) {
                            Some(branch) => branch,
                            None => match self.state.workers.get(&task.id) {
                                Some(worker) => worker.branch.clone(),
                                None => Some(crate::worktree::worker_branch(&task.id)),
                            },
                        }
                    } else {
                        None
                    };
                    wants.push(Want {
                        kind: JudgeKind::Review,
                        tasks: vec![task.id.clone()],
                        title: format!("review {} {}", task.id, task.title),
                        since: task.updated_at,
                        review: Some(ReviewWant {
                            pr,
                            mode: ReviewMode::PreMerge {
                                no_code,
                                worker_branch,
                                parked_at: parked_in_review_at(&detail),
                            },
                        }),
                    });
                }
            }
        }

        // Neither list needs the other, so both are read at once. A failure of the first is
        // the one reported, as when they were read in turn.
        let proposals = TaskQuery {
            board: Some(slug.clone()),
            proposal: Some(true),
            limit: Some(board_proto::TASKS_LIMIT_MAX),
            ..TaskQuery::default()
        };
        let ready = TaskQuery {
            board: Some(slug.clone()),
            ready: Some(true),
            proposal: Some(false),
            limit: Some(board_proto::TASKS_LIMIT_MAX),
            ..TaskQuery::default()
        };
        // The ready tasks serve the triage and the goal review alone.
        let ready = async {
            if all {
                client.tasks(&ready).await
            } else {
                Ok(Vec::new())
            }
        };
        let (proposals, ready) = tokio::join!(client.tasks(&proposals), ready);
        let proposals = proposals.context("listing the pending proposals")?;
        let ready = ready.context("listing the ready tasks")?;
        let mut by_parent: BTreeMap<Option<TaskId>, Vec<&Task>> = BTreeMap::new();
        for proposal in proposals.iter().filter(|t| is_proposal(t)) {
            by_parent
                .entry(proposal.parent.clone())
                .or_default()
                .push(proposal);
        }
        for (parent, mut group) in by_parent {
            // One judge per parent task at a time.
            if group
                .iter()
                .any(|t| at_work.judges(JudgeKind::Verdict, &t.id))
            {
                continue;
            }
            group.sort_by_key(|t| t.created_at);
            let batch: Vec<&Task> = group
                .into_iter()
                .filter(|t| !self.judge_waits(&retry_key(JudgeKind::Verdict, &t.id), t))
                .take(JUDGE_TASKS_AT_ONCE)
                .collect();
            let Some(first) = batch.first() else {
                continue;
            };
            let under = parent.map_or_else(|| "no parent".to_owned(), |p| format!("under {p}"));
            wants.push(Want {
                kind: JudgeKind::Verdict,
                title: format!("verdict {} ({under})", first.id),
                since: first.created_at,
                tasks: batch.iter().map(|t| t.id.clone()).collect(),
                review: None,
            });
        }

        if all && self.triages(slug) {
            let mut items: Vec<&Task> = ready
                .iter()
                .filter(|t| t.ready && t.feedback.is_some())
                .filter(|t| !at_work.judges(JudgeKind::Triage, &t.id))
                .collect();
            items.sort_by_key(|t| t.created_at);
            items.retain(|t| !self.judge_waits(&retry_key(JudgeKind::Triage, &t.id), t));
            for batch in items.chunks(JUDGE_TASKS_AT_ONCE) {
                wants.push(Want {
                    kind: JudgeKind::Triage,
                    title: format!("triage {}", batch[0].id),
                    since: batch[0].created_at,
                    tasks: batch.iter().map(|t| t.id.clone()).collect(),
                    review: None,
                });
            }
        }

        // The goal review comes last: once a task was finished and the board is at rest.
        let work_left = ready
            .iter()
            .any(|t| t.ready && t.auto && t.feedback.is_none());
        if all
            && !board.goal.trim().is_empty()
            && let Some(last_done) = self.state.goal_due.get(slug)
            && in_progress == 0
            && !work_left
            && in_review.is_empty()
            && wants.is_empty()
            && at_work.count == 0
            && !self.state.judge_retry.contains_key(&goal_key(slug))
        {
            wants.push(Want {
                kind: JudgeKind::Goal,
                tasks: last_done.iter().cloned().collect(),
                title: format!("goal review {slug}"),
                since: now,
                review: None,
            });
        }

        // The board asks again below if it still wants one of the device's slots.
        let mut waited = self.turns(judging).take(slug);
        // A board at its usage stop gets no new judge, as it gets no new worker.
        if self.usage_holds(slug, now) {
            let hold_key = format!("usage_hold judge {slug}");
            if !self.state.holds.contains_key(&hold_key) {
                self.record_drop(
                    "usage_hold",
                    Some(slug),
                    None,
                    Some(agent),
                    &format!(
                        "usage holds judge dispatch on {slug} (usage_stop_at={})",
                        board.usage_stop_at
                    ),
                    serde_json::json!({
                        "kind": "judge",
                        "board": slug,
                        "usage_stop_at": board.usage_stop_at,
                    }),
                    now,
                );
                self.state.holds.insert(hold_key, now);
            }
            return self.save_at(now);
        }
        self.hold_ended("usage_hold", &format!("judge {slug}"));
        wants.sort_by_key(|w| w.since);
        let free = JUDGES_AT_ONCE.saturating_sub(at_work.count);
        if wants.len() > free {
            tracing::debug!(
                "{slug}: {} judgments wait for one of the {JUDGES_AT_ONCE} judges at once",
                wants.len() - free
            );
        }
        // A start that fails leaves its slot to the next judgment that waits.
        let mut started = 0;
        for want in wants {
            if started == free {
                break;
            }
            // The device's limit holds across boards, and the board that waited longest
            // for a slot gets the next one.
            self.read_running().await;
            self.sync_resource_ledger(now);
            let flat_limit = self.daemon_run_limit();
            let verdicts = self.home_verdict_runs();
            let capped = !all && verdicts >= HOME_VERDICTS_AT_ONCE;
            let needed_units = device::cost_units_for_judge(want.kind);
            let weighted_mode = self.ctx.config.weight_units.is_some();
            let weighted_held = self.weighted_full(needed_units);
            let flat_held = !weighted_mode && self.runs() >= flat_limit;
            let full = self.load_unknown().is_some() || flat_held || weighted_held;
            if capped || full || self.turns(judging).another_first(slug, waited) {
                // The limit that holds it, and that limit's numbers. Off autopilot, a
                // board that waits its turn while the device has a free run waits for
                // the boards that this cap held before it.
                let (limit, of, cause) = if capped || (!all && !full) {
                    let cause = format!(
                        "{verdicts} verdict judges run on the boards that are not on autopilot"
                    );
                    let of = "verdict judges at once of the boards that are not on autopilot";
                    (HOME_VERDICTS_AT_ONCE, of, cause)
                } else if weighted_mode || (!flat_held && weighted_held) {
                    let (max_units, cause) = self.weighted_load(needed_units);
                    (max_units, "weight units of this device", cause)
                } else {
                    (flat_limit, "runs of this device", self.load())
                };
                if self.turns(judging).wait(slug, waited, now) {
                    tracing::info!(
                        "{slug}: `{}` waits for one of the {limit} {of}; {cause}",
                        want.title
                    );
                }
                // A held judge gets no note, since a note on an owned task wakes its
                // owner: the device's trail file is its record.
                self.record_hold("judge", slug, &want.title, limit, &cause, now);
                break;
            }
            let names = want.title.clone();
            match self
                .start_judge(board, &client, repo_key.as_deref(), &project, want, now)
                .await
            {
                Ok(StartOutcome::Started) => {
                    self.hold_ended("judge", &names);
                    self.hold_ended("review_wait", &names);
                    started += 1;
                    // Served: a further judge of this board waits behind the others.
                    waited = None;
                }
                Ok(StartOutcome::Waited) => {}
                Err(err) => {
                    self.hold_ended("judge", &names);
                    self.hold_ended("review_wait", &names);
                    tracing::warn!("starting the judge `{names}`: {err:#}");
                    if let Some(refusal) = crate::worktree::privacy_refusal(&err) {
                        let repo_keys = super::repo_keys_for_run(repo_key.as_deref(), board, None);
                        self.record_tcc_repo_backoff(
                            &board.slug,
                            None,
                            None,
                            &repo_keys,
                            &project,
                            &refusal.detail,
                            now,
                        );
                        if judging == Judging::All {
                            self.release(&board.slug).await;
                            self.dispatch_renewed.remove(&board.slug);
                        }
                        break;
                    }
                }
            }
        }
        self.save()
    }

    /// The queue of the boards that wait for a judge's slot: the boards off autopilot
    /// have their own.
    fn turns(&mut self, judging: Judging) -> &mut device::Turns {
        match judging {
            Judging::All => &mut self.judge_turns,
            Judging::Verdicts => &mut self.verdict_turns,
        }
    }

    /// Whether `task` still waits out the retry of `key`: its judge ended less than
    /// [`JUDGE_RETRY`] ago and the task has not changed since.
    fn judge_waits(&self, key: &str, task: &Task) -> bool {
        self.state
            .judge_retry
            .get(key)
            .is_some_and(|retry| retry.updated_at.is_none_or(|at| at == task.updated_at))
    }

    fn put_run(&mut self, run: &JudgeRun) {
        match self.state.judges.iter_mut().find(|r| r.same(run)) {
            Some(known) => *known = run.clone(),
            None => self.state.judges.push(run.clone()),
        }
    }

    /// Forgets `run`, cleans up its daemon-managed worktree when any, and keeps its tasks
    /// from another judge of its kind for `wait` unless they change.
    async fn end_run(&mut self, run: &JudgeRun, client: &Client, now: Timestamp, wait: TimeDelta) {
        self.state.judges.retain(|r| !r.same(run));
        if let Some(run_id) = run.run_id.as_deref() {
            let _ = self
                .db
                .update_local_run_state(run_id, LocalRunState::Finished, None, now);
        } else if let Some(thread) = run.thread.as_deref() {
            let _ = self
                .db
                .finish_local_run_by_session(thread, LocalRunState::Finished, now);
        }
        self.sync_resource_ledger(now);
        if let Some(wt_id) = run.worktree_id.as_deref() {
            self.cleanup_worktree(wt_id, now).await;
        }
        let until = now + wait;
        if run.kind == JudgeKind::Goal || run.tasks.is_empty() {
            let retry = JudgeRetry {
                until,
                updated_at: None,
            };
            self.state.judge_retry.insert(goal_key(&run.board), retry);
            return;
        }
        // At most [`JUDGE_TASKS_AT_ONCE`] tasks, each read apart from the others.
        let reads = futures_util::future::join_all(run.tasks.iter().map(|t| client.task(t))).await;
        for (task, read) in run.tasks.iter().zip(reads) {
            let updated_at = match read {
                Ok(detail) => Some(detail.task.updated_at),
                Err(err) => {
                    tracing::debug!("reading {task} after its judge: {err:#}");
                    None
                }
            };
            self.state
                .judge_retry
                .insert(run.run_retry_key(task), JudgeRetry { until, updated_at });
        }
    }

    /// Finishes the starts on `board` that did not: finds the thread of one that did not
    /// record it, registers a judge the server does not have, and notes a review judge's
    /// thread on its task.
    async fn settle_judge_starts(
        &mut self,
        board: &Board,
        client: &Client,
        project: &Path,
        now: Timestamp,
    ) {
        let unsettled: Vec<JudgeRun> = self
            .state
            .judges
            .iter()
            .filter(|r| r.board == board.slug)
            .filter(|r| !r.registered || (r.kind == JudgeKind::Review && !r.noted))
            .cloned()
            .collect();
        for mut run in unsettled {
            if run.thread.is_none() {
                match self.find_judge_thread(board, project, &run).await {
                    Ok(Some(thread)) => {
                        if let Some(run_id) = run.run_id.as_deref() {
                            let _ = self.db.update_local_run_state(
                                run_id,
                                LocalRunState::Running,
                                Some(&thread),
                                now,
                            );
                        }
                        run.thread = Some(thread);
                    }
                    // No judge ran: the start is tried again soon.
                    Ok(None) => {
                        self.end_run(&run, client, now, JUDGE_START_RETRY).await;
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!("{} judge of {}: {err:#}", run.kind_name(), run.names());
                        continue;
                    }
                }
            }
            if let Err(err) = self.register_run(client, &mut run).await {
                tracing::warn!(
                    "registering the {} judge of {}: {err:#}",
                    run.kind_name(),
                    run.names()
                );
                if now - run.started_at >= JUDGE_REGISTER_WAIT {
                    self.end_run(&run, client, now, JUDGE_RETRY).await;
                    continue;
                }
            }
            self.put_run(&run);
        }
    }

    /// The thread of a judge start that did not record it, as for a worker: the newest
    /// thread with its title created since, briefed when its message never reached it.
    async fn find_judge_thread(
        &self,
        board: &Board,
        project: &Path,
        run: &JudgeRun,
    ) -> Result<Option<String>> {
        if run.runner.as_deref().unwrap_or("t3") != "t3" {
            return Ok(None);
        }
        let found = self
            .t3
            .threads_titled(project, &run.title)
            .await
            .context("looking for the thread of an unfinished judge start")?
            .into_iter()
            .filter(|t| t.created_at >= run.started_at && thread_agent(&t.id).is_ok())
            .max_by_key(|t| t.created_at);
        let Some(found) = found else {
            tracing::warn!(
                "the start of the {} judge of {} at {} made no thread",
                run.kind_name(),
                run.names(),
                run.started_at
            );
            return Ok(None);
        };
        if !found.ran {
            let brief = crate::brief::judge_brief_with_worktree(
                run.run_brief_kind(),
                &board.slug,
                &run.tasks,
                run.worktree_note.as_deref(),
            );
            let sent = if self.runner.kind() == "t3" {
                self.runner
                    .send(&RunHandle::t3(&found.id), &SteerMessage::new(&brief))
                    .await
            } else {
                self.t3.send(&found.id, &brief).await
            };
            if let Err(err) = sent {
                tracing::warn!(
                    "briefing judge thread {} left by an earlier start: {err:#}; archive it in T3",
                    found.id
                );
                return Ok(None);
            }
        }
        Ok(Some(found.id))
    }

    /// Registers the judge of `run` with the server, then notes a review judge's thread on
    /// its task.
    async fn register_run(&self, client: &Client, run: &mut JudgeRun) -> Result<()> {
        let Some(thread) = run.thread.clone() else {
            return Ok(());
        };
        let runner_kind = run.runner.as_deref().unwrap_or("t3");
        let judge = match &run.agent_id {
            Some(agent_id) => agent_id.clone(),
            None => RunHandle::new(runner_kind, &thread).resolve_agent_id()?,
        };
        if !run.registered {
            client
                .register_judge(
                    &run.board,
                    &RegisterJudge {
                        agent: judge.clone(),
                        kind: run.kind,
                        tasks: run.tasks.clone(),
                    },
                )
                .await?;
            run.registered = true;
        }
        if run.kind == JudgeKind::Review && !run.noted {
            let link = if runner_kind == "t3" {
                match self.t3.thread_link(&thread).await {
                    Ok(Some(link)) => link,
                    _ => format!("thread {thread}"),
                }
            } else {
                format!("{runner_kind} run {thread}")
            };
            let Some(agent) = client.agent() else {
                return Ok(());
            };
            // `reviewed` reads the judge's agent id at the end of this note.
            for task in &run.tasks {
                let text = if run.after_merge {
                    format!("{JUDGE_NOTE} {link} (after-merge) (agent {judge})")
                } else {
                    format!("{JUDGE_NOTE} {link} (agent {judge})")
                };
                match add_note(client, task, agent, NoteKind::Note, text).await {
                    Ok(()) => run.noted = true,
                    Err(err) => tracing::warn!("noting judge thread {thread} on {task}: {err:#}"),
                }
            }
        }
        Ok(())
    }

    /// Ends the runs on `slug` whose judge the server no longer lists as running.
    async fn end_judges(
        &mut self,
        slug: &BoardSlug,
        client: &Client,
        listed: &[Judge],
        now: Timestamp,
    ) {
        let ended: Vec<JudgeRun> = self
            .state
            .judges
            .iter()
            .filter(|r| r.board == *slug && r.registered)
            .filter(|r| {
                let runner_kind = r.runner.as_deref().unwrap_or("t3");
                let judge = r.agent_id.clone().or_else(|| {
                    r.thread
                        .as_deref()
                        .and_then(|t| RunHandle::new(runner_kind, t).resolve_agent_id().ok())
                });
                !listed
                    .iter()
                    .any(|j| j.running && Some(&j.agent) == judge.as_ref())
            })
            .cloned()
            .collect();
        for run in ended {
            tracing::info!(
                "the {} judge of {} on {slug} ended",
                run.kind_name(),
                run.names()
            );
            self.end_run(&run, client, now, JUDGE_RETRY).await;
        }
    }

    /// Prepares the worktree and brief worktree sentence for an after-merge review judge
    /// (shared by both legacy and pull dispatch), logging and surfacing any base-branch
    /// fetch fallback so the judge never silently checks or deploys from the wrong branch.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn prepare_after_merge_worktree(
        &self,
        task: &TaskId,
        project: &Path,
        pr_url: Option<&str>,
        recorded_base: Option<&str>,
        merge_commit: Option<&str>,
        default_base: Option<&str>,
        now: Timestamp,
    ) -> Result<(Option<crate::t3::NewWorktree>, Option<String>)> {
        use crate::worktree::{ReviewBase, ReviewTarget};
        let (pr_base, view_err) = if let Some(url) = pr_url {
            match self.gh.view(project, url).await {
                Ok(view) if !view.base_ref.is_empty() => (Some(view.base_ref), None),
                Ok(_) => (recorded_base.map(ToOwned::to_owned), None),
                Err(err) => (
                    recorded_base.map(ToOwned::to_owned),
                    Some(format!("reading {url}: {err:#}")),
                ),
            }
        } else {
            (recorded_base.map(ToOwned::to_owned), None)
        };
        if pr_base.is_none()
            && let Some(err) = view_err.as_deref()
        {
            tracing::warn!(
                "after-merge review of {task}: {err}; base branch unknown, falling back to default base"
            );
        }
        let target = match pr_base.as_deref() {
            Some(branch) => ReviewTarget::PullRequest(branch),
            None => ReviewTarget::None,
        };
        let prepared =
            crate::worktree::review_worktree_with_base(project, task, target, default_base, now)
                .await
                .context("preparing after-merge review judge worktree")?;
        Ok(match prepared {
            None => (None, None),
            Some(prepared) => {
                let fallback = if prepared.source == ReviewBase::DefaultBase {
                    if let Some(expected) = pr_base.as_deref() {
                        let reason = prepared
                            .fetch_error
                            .as_deref()
                            .unwrap_or("branch could not be fetched from origin");
                        tracing::warn!(
                            "after-merge review of {task}: could not fetch base branch `{expected}` ({reason}); starting from {}",
                            prepared.worktree.base
                        );
                        Some((expected, reason))
                    } else if let Some(err) = view_err.as_deref() {
                        Some(("merged PR base branch", err))
                    } else {
                        prepared
                            .fetch_error
                            .as_deref()
                            .map(|err| (prepared.worktree.base.as_str(), err))
                    }
                } else {
                    None
                };
                let note = crate::brief::after_merge_worktree_note(
                    &prepared.worktree.base,
                    pr_url,
                    merge_commit,
                    fallback,
                );
                (Some(prepared.worktree), Some(note))
            }
        })
    }

    /// Starts a judge thread for `want`: records it before the start, so a start the
    /// daemon does not live to finish is found again, then registers its agent as a judge
    /// of the board. The agent's id is known only once T3 made the thread, so the judge is
    /// registered right after the start, before its session makes its first board call.
    async fn start_judge(
        &mut self,
        board: &Board,
        client: &Client,
        repo_key: Option<&str>,
        project: &Path,
        want: Want,
        now: Timestamp,
    ) -> Result<StartOutcome> {
        let after_merge = matches!(
            want.review.as_ref().map(|r| &r.mode),
            Some(ReviewMode::AfterMerge { .. })
        );
        let want_brief_kind = if want.kind == JudgeKind::Review && after_merge {
            crate::brief::JudgeKind::AfterMerge
        } else {
            brief_kind(want.kind)
        };
        let (worktree, worktree_note) = match (want.kind, want.tasks.first(), &want.review) {
            (
                JudgeKind::Review,
                Some(task),
                Some(ReviewWant {
                    pr,
                    mode:
                        ReviewMode::AfterMerge {
                            base_ref,
                            merge_commit,
                        },
                }),
            ) => {
                let default_base = self.ctx.config.base_branch(&board.slug, repo_key);
                self.prepare_after_merge_worktree(
                    task,
                    project,
                    pr.as_deref(),
                    base_ref.as_deref(),
                    merge_commit.as_deref(),
                    default_base,
                    now,
                )
                .await?
            }
            (
                JudgeKind::Review,
                Some(task),
                Some(ReviewWant {
                    pr,
                    mode:
                        ReviewMode::PreMerge {
                            no_code,
                            worker_branch,
                            parked_at,
                        },
                }),
            ) => {
                use crate::worktree::{ReviewBase, ReviewTarget};
                let (pr_ref, fallback_reason) = if let Some(pr_url) = pr {
                    match self.gh.view(project, pr_url).await {
                        // A merged pull request's head branch may be deleted or pre-squash:
                        // review the landed code on its base branch instead.
                        Ok(pr) if pr.state == PrState::Merged => {
                            if !pr.base_ref.is_empty() {
                                (Some(pr.base_ref), None)
                            } else {
                                (
                                    None,
                                    Some(format!(
                                        "merged pull request {pr_url} has no base branch"
                                    )),
                                )
                            }
                        }
                        // A fork's branch is not on `origin`.
                        Ok(pr) if !pr.cross_repository => {
                            if !pr.head_ref.is_empty() {
                                (Some(pr.head_ref), None)
                            } else {
                                (
                                    None,
                                    Some(format!("pull request {pr_url} has no head branch")),
                                )
                            }
                        }
                        Ok(_) => (
                            None,
                            Some(format!(
                                "pull request {pr_url} is from a fork and not on origin"
                            )),
                        ),
                        Err(err) => (None, Some(format!("reading {pr_url}: {err:#}"))),
                    }
                } else if *no_code {
                    (None, Some("task notes declare pr: none".to_owned()))
                } else {
                    (None, None)
                };
                let target = if let Some(branch) = pr_ref.as_deref() {
                    ReviewTarget::PullRequest(branch)
                } else if pr.is_none() && !*no_code {
                    match worker_branch.as_deref() {
                        Some(branch) => ReviewTarget::TaskBranch(branch),
                        None => ReviewTarget::None,
                    }
                } else {
                    ReviewTarget::None
                };
                let waiting_without_target = pr.is_none()
                    && !*no_code
                    && matches!(target, ReviewTarget::None)
                    && within_pr_wait(*parked_at, now);
                if waiting_without_target
                    && crate::worktree::is_repo_top(project)
                        .await
                        .context("checking the review judge's repository")?
                {
                    let until = *parked_at + REVIEW_PR_WAIT;
                    let cause = format!(
                        "{task} has no PR recorded yet and no worker branch; waiting until {until} ({}s from park) before starting on the default base",
                        REVIEW_PR_WAIT.num_seconds()
                    );
                    self.record_hold("review_wait", &board.slug, &want.title, 0, &cause, now);
                    tracing::info!("review of {task}: {cause}");
                    return Ok(StartOutcome::Waited);
                }
                let default_base = self.ctx.config.base_branch(&board.slug, repo_key);
                match crate::worktree::review_worktree_with_base(
                    project,
                    task,
                    target,
                    default_base,
                    now,
                )
                .await
                .context("preparing the review judge's worktree")?
                {
                    None => (None, None),
                    Some(prepared) => match prepared.source {
                        ReviewBase::PullRequest => (Some(prepared.worktree), None),
                        ReviewBase::TaskBranch => {
                            tracing::info!(
                                "review of {task}: no PR recorded yet; starting on worker branch {}",
                                prepared.worktree.base
                            );
                            let note =
                                crate::brief::review_task_branch_note(&prepared.worktree.base);
                            (Some(prepared.worktree), Some(note))
                        }
                        ReviewBase::DefaultBase => {
                            if pr.is_none() && !*no_code && within_pr_wait(*parked_at, now) {
                                let until = *parked_at + REVIEW_PR_WAIT;
                                let branch_desc = worker_branch
                                    .as_deref()
                                    .map(|b| format!("worker branch `{b}` is not on origin yet"))
                                    .unwrap_or_else(|| "no worker branch is recorded".to_owned());
                                let cause = format!(
                                    "{task} has no PR recorded yet and {branch_desc}; waiting until {until} ({}s from park) before starting on {}",
                                    REVIEW_PR_WAIT.num_seconds(),
                                    prepared.worktree.base
                                );
                                self.record_hold(
                                    "review_wait",
                                    &board.slug,
                                    &want.title,
                                    0,
                                    &cause,
                                    now,
                                );
                                tracing::info!("review of {task}: {cause}");
                                return Ok(StartOutcome::Waited);
                            }
                            let reason = prepared
                                .fetch_error
                                .or(fallback_reason)
                                .unwrap_or_else(|| "no PR or worker branch resolved".to_owned());
                            tracing::warn!(
                                "review of {task}: {reason}; starting from {}",
                                prepared.worktree.base
                            );
                            let note = crate::brief::review_default_base_note(
                                &prepared.worktree.base,
                                *no_code,
                            );
                            (Some(prepared.worktree), Some(note))
                        }
                    },
                }
            }
            _ => (None, None),
        };
        if worktree.is_some() {
            let repo_keys = super::repo_keys_for_run(repo_key, board, None);
            self.clear_tcc_repo_backoff(&repo_keys, project);
        }
        let brief = crate::brief::judge_brief_with_options(
            want_brief_kind,
            &board.slug,
            &want.tasks,
            worktree_note.as_deref(),
            self.runner.kind() == "t3",
        );
        let run_label = want
            .tasks
            .first()
            .map_or_else(|| board.slug.to_string(), ToString::to_string);
        let run_id = format!(
            "judge-{}-{run_label}-{}",
            want_brief_kind.as_str(),
            now.format("%Y%m%d-%H%M%S")
        );
        let cost_units = device::cost_units_for_judge(want.kind);
        let intended_branch = worktree.as_ref().map(|w| w.branch.clone());

        let preassigned_agent_id = (self.runner.kind() == "acp")
            .then(|| format!("antigravity:{run_id}").parse::<AgentId>().ok())
            .flatten();
        // Write-before-act: commit `local_runs` intent and judge record into `daemon.db`
        // BEFORE creating any git worktree or spawning the runner (agent-board-539).
        let mut run = JudgeRun {
            board: board.slug.clone(),
            kind: want.kind,
            tasks: want.tasks,
            thread: None,
            runner: Some(self.runner.kind().to_owned()),
            agent_id: preassigned_agent_id.clone(),
            worktree_id: None,
            worktree_path: None,
            run_id: Some(run_id.clone()),
            title: want.title,
            started_at: now,
            registered: false,
            noted: false,
            worktree_note,
            after_merge,
        };
        let mut local_run = LocalRunRecord {
            run_id: run_id.clone(),
            task_id: run.tasks.first().cloned(),
            board: board.slug.clone(),
            kind: run.kind_name().to_owned(),
            runner: self.runner.kind().to_owned(),
            session_id: None,
            worktree_path: None,
            branch: intended_branch,
            cost_units,
            pid: None,
            state: LocalRunState::Intent,
            lease_expires_at: None,
            created_at: now,
            updated_at: now,
            meta_json: serde_json::to_string(&run).ok(),
        };
        self.db.upsert_local_run(&local_run)?;
        self.put_run(&run);
        if let Err(err) = self.save_at(now) {
            self.state.judges.retain(|r| !r.same(&run));
            let _ = self
                .db
                .update_local_run_state(&run_id, LocalRunState::Failed, None, now);
            return Err(err);
        }
        self.sync_resource_ledger(now);

        let (workdir, spec_worktree) = if self.daemon_worktrees
            && let Some(spec) = &worktree
        {
            match self
                .worktrees
                .create(project, repo_key, &run_id, spec, now)
                .await
            {
                Ok(managed) => {
                    run.worktree_id = Some(managed.id);
                    run.worktree_path = Some(managed.path.clone());
                    local_run.branch = Some(managed.branch);
                    local_run.worktree_path = Some(managed.path.clone());
                    local_run.state = LocalRunState::Starting;
                    local_run.updated_at = now;
                    local_run.meta_json = serde_json::to_string(&run).ok();
                    self.put_run(&run);
                    let persisted = self
                        .db
                        .upsert_local_run(&local_run)
                        .and_then(|()| self.save_at(now));
                    if let Err(err) = persisted {
                        self.state.judges.retain(|r| !r.same(&run));
                        let _ = self.db.update_local_run_state(
                            &run_id,
                            LocalRunState::Failed,
                            None,
                            now,
                        );
                        self.sync_resource_ledger(now);
                        if let Some(wt_id) = run.worktree_id.as_deref() {
                            self.cleanup_worktree(wt_id, now).await;
                        }
                        return Err(err);
                    }
                    (managed.path, None)
                }
                Err(err) => {
                    self.state.judges.retain(|r| !r.same(&run));
                    let _ =
                        self.db
                            .update_local_run_state(&run_id, LocalRunState::Failed, None, now);
                    self.sync_resource_ledger(now);
                    let _ = self.save_at(now);
                    return Err(err.context("creating daemon-managed judge worktree"));
                }
            }
        } else {
            local_run.state = LocalRunState::Starting;
            local_run.updated_at = now;
            self.db.upsert_local_run(&local_run)?;
            (project.to_path_buf(), worktree)
        };
        let model = self.ctx.config.model_for_judge(run.kind).cloned();
        let mut metadata = BTreeMap::from([
            ("board".to_owned(), board.slug.to_string()),
            ("kind".to_owned(), format!("judge:{}", run.kind_name())),
        ]);
        if let Some(agent_id) = &preassigned_agent_id {
            metadata.insert("agent_id".to_owned(), agent_id.to_string());
            // Register pre-assigned ACP judge before spawning so Call #1 already holds judge
            // rights even before `spawn()` returns; `register_run` retries if this fails.
            if client
                .register_judge(
                    &run.board,
                    &RegisterJudge {
                        agent: agent_id.clone(),
                        kind: run.kind,
                        tasks: run.tasks.clone(),
                    },
                )
                .await
                .is_ok()
            {
                run.registered = true;
            }
        }
        let spec = RunSpec {
            run_id: run_id.clone(),
            title: run.title.clone(),
            prompt: brief,
            model,
            worktree: spec_worktree,
            metadata,
        };
        let handle = match self.runner.spawn(&spec, &workdir).await {
            Ok(handle) => handle,
            Err(err) => {
                if let Some(wt_id) = run.worktree_id.as_deref() {
                    self.cleanup_worktree(wt_id, now).await;
                }
                return Err(err);
            }
        };
        let thread = handle.native_id.clone();
        let resolved_agent = if self.runner.kind() == "t3" {
            thread_agent(&thread)
                .with_context(|| format!("t3 started thread {thread:?}; archive it in T3"))?
        } else {
            handle.resolve_agent_id()?
        };
        if handle.runner == "t3" {
            tracing::info!(
                "started {} judge thread {thread} for {} on {}",
                run.kind_name(),
                run.names(),
                board.slug
            );
        } else {
            tracing::info!(
                "started {} judge {thread} ({}) for {} on {}",
                run.kind_name(),
                handle.runner,
                run.names(),
                board.slug
            );
        }
        run.thread = Some(thread.clone());
        run.runner = Some(handle.runner.clone());
        run.agent_id = Some(resolved_agent);
        local_run.session_id = Some(thread);
        local_run.runner = handle.runner;
        local_run.state = LocalRunState::Running;
        local_run.updated_at = now;
        local_run.meta_json = serde_json::to_string(&run).ok();
        self.db.upsert_local_run(&local_run)?;
        if run.kind == JudgeKind::Goal {
            self.state.goal_due.remove(&board.slug);
        }
        self.put_run(&run);
        self.sync_resource_ledger(now);
        self.save_at(now)?;
        // Tried again on the next reconcile.
        if let Err(err) = self.register_run(client, &mut run).await {
            tracing::warn!(
                "registering the {} judge of {}: {err:#}",
                run.kind_name(),
                run.names()
            );
        }
        self.put_run(&run);
        let _ = self.save_at(now);
        Ok(StartOutcome::Started)
    }

    /// Closes a task in `review` that a judge passed: merges its pull request once it can
    /// be merged, then either records an after-merge handoff (returning `Some`) when the
    /// pass listed `after merge:` checks or finishes the task once its worker no longer runs.
    #[allow(clippy::too_many_arguments)]
    async fn close_reviewed(
        &mut self,
        board: &Board,
        agent: &AgentId,
        client: &Client,
        project: &Path,
        detail: &TaskDetail,
        pass: &Note,
        now: Timestamp,
    ) -> Result<Option<AfterMergeHandoff>> {
        let task = &detail.task;
        // Its worker may still write its report, or push to the pull request: nothing is
        // merged or finished while it runs.
        if let Some(owner) = task.owner.as_ref().filter(|owner| *owner != agent) {
            let running = match client.agent_info(owner).await {
                Ok(info) => info.state == AgentState::Running,
                Err(err) if error_code(&err) == Some(ErrorCode::NotFound) => false,
                Err(err) => return Err(err.context(format!("reading {owner}"))),
            };
            if running {
                return Ok(None);
            }
        }
        let left_checks = after_merge_checks(&pass.text);
        let mut merged = None;
        let passed = passed_pr(detail, pass, &board.repos);
        // Reject loop-capped after-merge passes BEFORE calling `advance_pr` so a capped
        // follow-up PR is never merged before being voided and reopened.
        if !left_checks.is_empty() {
            let planned_pr = match &passed {
                PassedPr::None => Some(None),
                PassedPr::Url(url) => Some(Some(url.as_str())),
                PassedPr::Unclear(_) => None,
            };
            if let Some(pr_opt) = planned_pr
                && after_merge_loop_detected(detail, pass.id, pr_opt)
            {
                let text = clip_note(format!(
                    "finding: the after-merge review pass still left post-merge checks unrun (`after merge:` lines remained after the merge): {}",
                    left_checks.join("; ")
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                reopen_passed_task(
                    agent,
                    client,
                    task,
                    "its after-merge review pass left post-merge checks unrun",
                )
                .await?;
                self.state.merges.remove(&task.id);
                return Ok(None);
            }
        }
        if passed != PassedPr::None {
            let parked_at = parked_in_review_at(detail);
            let mut merge = match self.state.merges.get(&task.id) {
                // A clock set back, or a fresh review round, reads at once.
                Some(merge)
                    if merge.read_at >= parked_at
                        && (TimeDelta::zero()..PR_READ_EVERY).contains(&(now - merge.read_at)) =>
                {
                    return Ok(None);
                }
                Some(merge) => {
                    let mut m = merge.clone();
                    m.reset_if_before(parked_at);
                    m
                }
                None => Merge::new(now),
            };
            merge.read_at = now;
            self.state.merges.insert(task.id.clone(), merge.clone());
            let url = match passed {
                PassedPr::Url(url) => url,
                PassedPr::Unclear(why) => {
                    tracing::warn!(
                        "{}: passed, but not merged or finished: {why}. Name its pull request in a `pr:` line of a new pass note",
                        task.id
                    );
                    return Ok(None);
                }
                PassedPr::None => unreachable!("checked above"),
            };
            let pr = self
                .gh
                .view(project, &url)
                .await
                .with_context(|| format!("reading {url}"))?;
            match advance_pr(
                agent,
                client,
                &*self.gh,
                project,
                detail,
                &url,
                pr,
                &mut self.state.merges,
                &mut merge,
                now,
            )
            .await?
            {
                AdvanceOutcome::Merged(pr) => merged = Some(pr),
                AdvanceOutcome::Reopened(_)
                | AdvanceOutcome::BlockedForReview { .. }
                | AdvanceOutcome::Deferred(_) => return Ok(None),
            }
        }
        if !left_checks.is_empty() {
            let current_pr = merged.as_ref().map(|pr| pr.url.as_str());
            if after_merge_loop_detected(detail, pass.id, current_pr) {
                let text = clip_note(format!(
                    "finding: the after-merge review pass still left post-merge checks unrun (`after merge:` lines remained after the merge): {}",
                    left_checks.join("; ")
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                reopen_passed_task(
                    agent,
                    client,
                    task,
                    "its after-merge review pass left post-merge checks unrun",
                )
                .await?;
                self.state.merges.remove(&task.id);
                return Ok(None);
            }
            let note_text = format_after_merge_ready_note(pass.id, merged.as_ref(), &left_checks);
            add_note(client, &task.id, agent, NoteKind::Note, note_text).await?;
            self.state.merges.remove(&task.id);
            tracing::info!(
                "{}: merged (or no PR); starting after-merge review judge for {} left check(s)",
                task.id,
                left_checks.len()
            );
            return Ok(Some(AfterMergeHandoff {
                pr: merged.as_ref().map(|pr| pr.url.clone()),
                base_ref: merged
                    .as_ref()
                    .and_then(|pr| (!pr.base_ref.is_empty()).then(|| pr.base_ref.clone())),
                merge_commit: merged.as_ref().and_then(|pr| pr.merge_commit.clone()),
            }));
        }
        let mut evidence = Vec::new();
        if let Some(pr) = &merged {
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
                .with_context(|| format!("handing {} to the dispatcher", task.id))?;
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
        self.state.merges.remove(&task.id);
        match &merged {
            Some(pr) => tracing::info!(
                "{}: finished after its review pass; {} is merged",
                task.id,
                pr.url
            ),
            None => tracing::info!(
                "{}: finished after its review pass; no pull request",
                task.id
            ),
        }
        Ok(None)
    }
}

/// Moves the pull request of a passed task toward its merge, shared by both legacy board-lease
/// dispatch and the pull-dispatch [`crate::deterministic_runner::DeterministicRunner`].
///
/// Conflicts and closed PRs immediately void the pass and reopen the task. Failed checks,
/// persistent drafts, and `MergeState::Waits` holds after checks pass are tracked per head
/// commit for [`PR_HOLD_WAIT`]: once that wait elapses on one commit, the dispatcher voids the
/// pass and reopens the task, or wakes the user with an `action` ask and moves the task to
/// `blocked` when only a human review is missing (`ReviewDecision::ReviewRequired`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn advance_pr(
    agent: &AgentId,
    client: &Client,
    gh: &dyn Gh,
    project: &Path,
    detail: &TaskDetail,
    url: &str,
    pr: PullRequest,
    merges: &mut BTreeMap<TaskId, Merge>,
    merge: &mut Merge,
    now: Timestamp,
) -> Result<AdvanceOutcome> {
    let task = &detail.task;
    match pr.state {
        PrState::Merged => {
            merge.clear_hold();
            return Ok(AdvanceOutcome::Merged(pr));
        }
        PrState::Closed => {
            let why = format!("its pull request {url} was closed without a merge");
            reopen_passed_task(agent, client, task, &why).await?;
            merges.remove(&task.id);
            return Ok(AdvanceOutcome::Reopened(why));
        }
        PrState::Open => {}
    }
    let short_head: String = pr.head_oid.chars().take(12).collect();
    let hold_mins = PR_HOLD_WAIT.num_minutes();
    let conflict = format!(
        "its pull request {url} conflicts with its base branch. Resolve the conflict on the pull request's branch `{}`",
        pr.head_ref
    );
    match (pr.merge_state, pr.draft, &pr.checks) {
        (MergeState::Conflict, _, _) => {
            reopen_passed_task(agent, client, task, &conflict).await?;
            merges.remove(&task.id);
            Ok(AdvanceOutcome::Reopened(conflict))
        }
        (_, true, _) => {
            let expired = merge.track_hold(MergeHoldKind::Draft, &pr.head_oid, now);
            merges.insert(task.id.clone(), merge.clone());
            if expired {
                let why = format!(
                    "its pull request {url} stayed a draft at commit {short_head} for {hold_mins} minutes. Mark it ready for review on branch `{}`",
                    pr.head_ref
                );
                reopen_passed_task(agent, client, task, &why).await?;
                merges.remove(&task.id);
                return Ok(AdvanceOutcome::Reopened(why));
            }
            match gh.ready(project, url).await {
                Ok(()) => {
                    tracing::info!("{}: {url} was a draft; marked it ready for review", task.id);
                    Ok(AdvanceOutcome::Deferred(
                        "marked draft PR ready for review; waiting for merge state and checks"
                            .to_owned(),
                    ))
                }
                Err(err) => {
                    tracing::warn!("{}: marking draft {url} ready failed: {err:#}", task.id);
                    Ok(AdvanceOutcome::Deferred(format!(
                        "marking draft PR ready failed: {err:#}"
                    )))
                }
            }
        }
        (MergeState::Behind, false, _) => {
            merge.clear_hold();
            merges.insert(task.id.clone(), merge.clone());
            match gh.update_branch(project, url).await? {
                Updated::Done => {
                    tracing::info!("{}: {url} was behind its base; updated its branch", task.id);
                    Ok(AdvanceOutcome::Deferred(
                        "updated PR branch from base; waiting for checks".to_owned(),
                    ))
                }
                Updated::Conflict => {
                    reopen_passed_task(agent, client, task, &conflict).await?;
                    merges.remove(&task.id);
                    Ok(AdvanceOutcome::Reopened(conflict))
                }
            }
        }
        (_, false, Checks::Failed(names)) => {
            let parked_at = parked_in_review_at(detail);
            let already_noted_on_board = detail.notes.iter().any(|n| {
                n.kind == NoteKind::Finding
                    && n.at >= parked_at
                    && n.text.contains(url)
                    && n.text.contains(&short_head)
            });
            let seen_locally = merge.failed_head.as_deref() == Some(pr.head_oid.as_str());
            if !seen_locally && !already_noted_on_board {
                let text = clip_note(format!(
                    "finding: the checks {} of {url} failed at commit {short_head}. The dispatcher merges it once every check passes.",
                    names.join(", "),
                ));
                add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
            }
            merge.failed_head = Some(pr.head_oid.clone());
            let expired = merge.track_hold(MergeHoldKind::FailedChecks, &pr.head_oid, now);
            merges.insert(task.id.clone(), merge.clone());
            if expired {
                let why = format!(
                    "the checks {} of {url} kept failing at commit {short_head} for {hold_mins} minutes. Fix the checks on branch `{}`",
                    names.join(", "),
                    pr.head_ref
                );
                reopen_passed_task(agent, client, task, &why).await?;
                merges.remove(&task.id);
                return Ok(AdvanceOutcome::Reopened(why));
            }
            Ok(AdvanceOutcome::Deferred(format!(
                "checks failed on {short_head}: {}",
                names.join(", ")
            )))
        }
        (MergeState::Clean, false, Checks::Passed) => {
            merge.clear_hold();
            merges.insert(task.id.clone(), merge.clone());
            gh.merge(project, url, &pr.head_oid)
                .await
                .with_context(|| format!("merging {url}"))?;
            tracing::info!("{}: merged {url}", task.id);
            let after = gh
                .view(project, url)
                .await
                .with_context(|| format!("re-reading {url} after merge"))?;
            if after.state == PrState::Merged {
                Ok(AdvanceOutcome::Merged(after))
            } else {
                Ok(AdvanceOutcome::Deferred(
                    "merge submitted; waiting for GitHub to report merged state".to_owned(),
                ))
            }
        }
        (MergeState::Waits, false, Checks::Passed) => match pr.review_decision {
            ReviewDecision::ReviewRequired => {
                let expired = merge.track_hold(MergeHoldKind::ReviewRequired, &pr.head_oid, now);
                merges.insert(task.id.clone(), merge.clone());
                if expired {
                    let (ask_id, reason) =
                        ask_for_human_review(agent, client, task, url, &short_head).await?;
                    merges.remove(&task.id);
                    return Ok(AdvanceOutcome::BlockedForReview { ask_id, reason });
                }
                Ok(AdvanceOutcome::Deferred(format!(
                    "waiting on required human review for {url} at {short_head}"
                )))
            }
            ReviewDecision::ChangesRequested => {
                let expired = merge.track_hold(MergeHoldKind::ChangesRequested, &pr.head_oid, now);
                merges.insert(task.id.clone(), merge.clone());
                if expired {
                    let why = format!(
                        "changes were requested on pull request {url} at commit {short_head} and held for {hold_mins} minutes. Address the review feedback on branch `{}`",
                        pr.head_ref
                    );
                    reopen_passed_task(agent, client, task, &why).await?;
                    merges.remove(&task.id);
                    return Ok(AdvanceOutcome::Reopened(why));
                }
                Ok(AdvanceOutcome::Deferred(format!(
                    "changes requested on {url} at {short_head}"
                )))
            }
            ReviewDecision::None | ReviewDecision::Approved => {
                let expired = merge.track_hold(MergeHoldKind::MergeWaits, &pr.head_oid, now);
                merges.insert(task.id.clone(), merge.clone());
                if expired {
                    let why = format!(
                        "GitHub kept holding the merge of {url} at commit {short_head} for {hold_mins} minutes after checks passed. Check the repository's merge rules on branch `{}`",
                        pr.head_ref
                    );
                    reopen_passed_task(agent, client, task, &why).await?;
                    merges.remove(&task.id);
                    return Ok(AdvanceOutcome::Reopened(why));
                }
                Ok(AdvanceOutcome::Deferred(
                    "merge held by repository rules after checks passed".to_owned(),
                ))
            }
        },
        (MergeState::Clean | MergeState::Waits, false, Checks::Pending) => {
            merge.clear_hold();
            merges.insert(task.id.clone(), merge.clone());
            Ok(AdvanceOutcome::Deferred("checks still pending".to_owned()))
        }
    }
}

/// Withdraws the pass of `task` and sets it `open`, so a new worker starts.
pub(crate) async fn reopen_passed_task(
    agent: &AgentId,
    client: &Client,
    task: &Task,
    why: &str,
) -> Result<()> {
    let text = clip_note(format!(
        "decision: review: void. The dispatcher set this task open for a new worker: {why}. Then park the task in review again."
    ));
    add_note(client, &task.id, agent, NoteKind::Decision, text).await?;
    client
        .task_action(&task.id, "release")
        .await
        .with_context(|| format!("setting {} open", task.id))?;
    tracing::info!("{}: set open: {why}", task.id);
    Ok(())
}

/// Creates (or reuses an existing open) action ask waking the user for a required human
/// review, then moves `task` from `review` to `blocked` naming that ask.
async fn ask_for_human_review(
    agent: &AgentId,
    client: &Client,
    task: &Task,
    url: &str,
    short_head: &str,
) -> Result<(i64, String)> {
    let existing = match client.asks(Some("open"), Some(&task.board), None).await {
        Ok(asks) => asks
            .into_iter()
            .find(|a| {
                a.task.as_ref() == Some(&task.id)
                    && a.agent.source() == AgentSource::Dispatch
                    && a.question.contains(url)
            })
            .map(|a| a.id),
        Err(err) => {
            tracing::debug!(
                "checking open asks on {} before asking for review: {err:#}",
                task.board
            );
            None
        }
    };
    let ask_id = match existing {
        Some(id) => id,
        None => {
            let mins = PR_HOLD_WAIT.num_minutes();
            let question = format!(
                "PR {url} for {} passed review and all checks at commit {short_head}, and has been waiting at least {mins} minutes for a required human review.\n\n\
                 Why it waits on you: Reason: only-me (only a human reviewer can approve the PR on GitHub).\n\n\
                 Options:\n\
                 - Approved — resume merge (Recommended): approve or merge {url} on GitHub, then choose this to return {} to review.\n\
                 - Reopen for a worker: void the review pass and set {} open for a new worker.",
                task.id, task.id, task.id
            );
            let created = client
                .create_ask(&CreateAsk {
                    agent: agent.clone(),
                    board: task.board.clone(),
                    task: Some(task.id.clone()),
                    kind: AskKind::Action,
                    question,
                    options: vec![
                        "Approved — resume merge".to_owned(),
                        "Reopen for a worker".to_owned(),
                    ],
                    change: None,
                })
                .await
                .with_context(|| format!("asking the user to review {url} for {}", task.id))?;
            created.ask.id
        }
    };
    let reason =
        format!("waiting on ask #{ask_id}: required human review on {url} at {short_head}");
    client
        .update_task(
            &task.id,
            &UpdateTask {
                agent: Some(agent.clone().into()),
                version: task.version,
                status: Some(TaskStatus::Blocked),
                title: None,
                body: None,
                acceptance: None,
                priority: None,
                blockers: None,
                blocked_reason: Some(reason.clone()),
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
        .with_context(|| format!("blocking {} on ask #{ask_id}", task.id))?;
    tracing::info!(
        "{}: blocked on ask #{ask_id} for required human review on {url}",
        task.id
    );
    Ok((ask_id, reason))
}

/// Handles an answered `dispatch:*` human-review ask on `task`, shared by both pull-mode
/// [`crate::deterministic_runner::DeterministicRunner`] and legacy [`Daemon`].
///
/// Acts only when `task` is still `blocked` on `ask #<ask_id>:` and has no other open ask.
/// Records the decision note first (unless `noted` is already true), then moves the task
/// via `update_task` to `open` (when reopening or when unowned) or back to `review` (when
/// approved and owned, preserving the standing `review: pass`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_dispatch_ask_answer(
    client: &Client,
    agent: &AgentId,
    task: &Task,
    ask_id: i64,
    asker_id: &AgentId,
    answer: &str,
    noted: &mut bool,
) -> Result<DispatchAskOutcome> {
    let reason_marker = format!("ask #{ask_id}:");
    let blocked_on_this_ask = task.status == TaskStatus::Blocked
        && task
            .blocked_reason
            .as_deref()
            .is_some_and(|r| r.contains(&reason_marker));
    if !blocked_on_this_ask {
        return Ok(DispatchAskOutcome::Noop(format!(
            "{} is no longer blocked on ask #{ask_id}",
            task.id
        )));
    }
    if let Ok(open_asks) = client.asks(Some("open"), Some(&task.board), None).await
        && open_asks.iter().any(|a| a.task.as_ref() == Some(&task.id))
    {
        return Ok(DispatchAskOutcome::Noop(format!(
            "{} still has another open ask",
            task.id
        )));
    }
    let reopen_for_worker = answer_requests_reopen(answer) || task.owner.is_none();
    if !*noted {
        let text = if reopen_for_worker {
            clip_note(format!(
                "decision: review: void. The user answered ask #{ask_id} from {asker_id}: {answer}. Setting task open for a new worker."
            ))
        } else {
            clip_note(format!(
                "decision: the user answered ask #{ask_id} from {asker_id}: {answer}"
            ))
        };
        add_note(client, &task.id, agent, NoteKind::Decision, text).await?;
        *noted = true;
    }
    let target_status = if reopen_for_worker {
        TaskStatus::Open
    } else {
        TaskStatus::Review
    };
    client
        .update_task(
            &task.id,
            &UpdateTask {
                agent: Some(agent.clone().into()),
                version: task.version,
                status: Some(target_status),
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
        .with_context(|| {
            format!(
                "moving {} to {} after ask #{ask_id}",
                task.id,
                if reopen_for_worker { "open" } else { "review" }
            )
        })?;
    if reopen_for_worker {
        tracing::info!("{}: ask #{ask_id} answered with reopen; set open", task.id);
        Ok(DispatchAskOutcome::Reopened)
    } else {
        tracing::info!("{}: ask #{ask_id} answered; returned to review", task.id);
        Ok(DispatchAskOutcome::ResumedReview)
    }
}

impl JudgeRun {
    fn run_brief_kind(&self) -> crate::brief::JudgeKind {
        if self.kind == JudgeKind::Review && self.after_merge {
            crate::brief::JudgeKind::AfterMerge
        } else {
            brief_kind(self.kind)
        }
    }

    fn run_retry_key(&self, task: &TaskId) -> String {
        if self.kind == JudgeKind::Review && self.after_merge {
            after_merge_retry_key(task)
        } else {
            retry_key(self.kind, task)
        }
    }

    fn kind_name(&self) -> &'static str {
        self.run_brief_kind().as_str()
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

    fn detail(notes: Vec<Note>, history: serde_json::Value) -> TaskDetail {
        serde_json::from_value(serde_json::json!({
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

    fn parked(at: &str) -> serde_json::Value {
        serde_json::json!({
            "seq": 9, "at": at, "board": "factory", "actor": "t3:worker",
            "type": "task_updated", "task": "factory-7",
            "payload": {"from": "doing", "to": "review"},
        })
    }

    #[test]
    fn a_note_records_a_verdict_only_at_its_start() {
        assert_eq!(
            review_verdict("review: pass. Done."),
            Some(ReviewVerdict::Pass)
        );
        assert_eq!(
            review_verdict("decision: review: PASS"),
            Some(ReviewVerdict::Pass)
        );
        assert_eq!(review_verdict(" review:fail x"), Some(ReviewVerdict::Fail));
        assert_eq!(review_verdict("review: void. y"), Some(ReviewVerdict::Void));
        assert_eq!(review_verdict("goal review: pass"), None);
        assert_eq!(review_verdict("the review: pass"), None);
        assert_eq!(review_verdict("review: later"), None);
    }

    #[test]
    fn a_pass_stands_until_the_task_is_parked_again_or_it_is_withdrawn() {
        let pass = note(
            2,
            "t3:judge",
            NoteKind::Decision,
            "review: pass",
            "2026-11-08T12:00:00Z",
        );
        let me: AgentId = "dispatch:mac".parse().unwrap();
        let started = |by: &str| {
            let text = format!("{JUDGE_NOTE} thread j (agent t3:judge)");
            note(1, by, NoteKind::Note, &text, "2026-11-08T11:30:00Z")
        };
        let judge = started("dispatch:mac");
        let reviewed = |detail: &TaskDetail| match reviewed(detail, &me) {
            Reviewed::Passed(note) | Reviewed::NeedsAfterMerge(note) => Some(Some(note.id)),
            Reviewed::Withdrawn => Some(None),
            Reviewed::Waits => None,
        };
        let before = serde_json::json!([parked("2026-11-08T11:00:00Z")]);
        let passed = detail(vec![judge.clone(), pass.clone()], before.clone());
        assert_eq!(reviewed(&passed), Some(Some(2)));

        // A pass by an agent the dispatcher did not start as this task's judge is none,
        // whoever else names it one.
        assert_eq!(reviewed(&detail(vec![pass.clone()], before.clone())), None);
        let claimed = detail(vec![started("t3:other"), pass.clone()], before.clone());
        assert_eq!(reviewed(&claimed), None);

        // Parked again after the pass: a new review.
        let again = detail(
            vec![judge.clone(), pass.clone()],
            serde_json::json!([parked("2026-11-08T13:00:00Z")]),
        );
        assert_eq!(reviewed(&again), None);

        // No verdict, a plain note, the owner's own pass, or a later fail: no pass.
        assert_eq!(reviewed(&detail(vec![], before.clone())), None);
        let plain = note(
            2,
            "t3:judge",
            NoteKind::Note,
            "review: pass",
            "2026-11-08T12:00:00Z",
        );
        assert_eq!(
            reviewed(&detail(vec![judge.clone(), plain], before.clone())),
            None
        );
        let own = note(
            2,
            "t3:worker",
            NoteKind::Decision,
            "review: pass",
            "2026-11-08T12:00:00Z",
        );
        assert_eq!(
            reviewed(&detail(vec![judge.clone(), own], before.clone())),
            None
        );
        let fail = note(
            3,
            "t3:judge2",
            NoteKind::Decision,
            "review: fail: the tests",
            "2026-11-08T12:30:00Z",
        );
        assert_eq!(
            reviewed(&detail(
                vec![judge.clone(), pass.clone(), fail],
                before.clone()
            )),
            None
        );
        let void = note(
            3,
            "dispatch:mac",
            NoteKind::Decision,
            "review: void. Conflict.",
            "2026-11-08T12:30:00Z",
        );
        assert_eq!(
            reviewed(&detail(vec![judge, pass, void], before)),
            Some(None)
        );
    }

    #[test]
    fn the_pass_note_gives_the_pull_request_the_checks_and_the_summary() {
        let text = "review: pass. The limit is now 80 characters.\nSee https://github.com/o/r/pull/3 for the earlier try.\n- check: cargo test --workspace => 0\ncheck: `cargo fmt --all -- --check` => exit 0\ncheck: no code\npr: https://github.com/o/r/pull/12";
        let pass = note(
            5,
            "t3:judge",
            NoteKind::Decision,
            text,
            "2026-11-08T12:00:00Z",
        );
        let parking = note(
            1,
            "t3:worker",
            NoteKind::Note,
            "note: parked, PR https://github.com/o/r/pull/11",
            "2026-11-08T11:00:00Z",
        );
        let with = detail(vec![parking.clone(), pass.clone()], serde_json::json!([]));
        let repos = ["github.com/o/r".to_owned()];
        let url = |n: u32| PassedPr::Url(format!("https://github.com/o/r/pull/{n}"));
        assert_eq!(passed_pr(&with, &pass, &repos), url(12));
        let checks = passed_checks(text);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].value, "cargo test --workspace");
        assert_eq!(checks[0].exit_code, Some(0));
        assert_eq!(checks[1].value, "cargo fmt --all -- --check");
        assert_eq!(
            passed_summary(text),
            "The limit is now 80 characters.\nSee https://github.com/o/r/pull/3 for the earlier try."
        );

        // Without a `pr:` line: the pass note's link, else the newest note before it.
        let linked = note(
            5,
            "t3:judge",
            NoteKind::Decision,
            "review: pass. Merged in https://github.com/o/r/pull/12.",
            "2026-11-08T12:00:00Z",
        );
        let with = detail(vec![parking.clone(), linked.clone()], serde_json::json!([]));
        assert_eq!(passed_pr(&with, &linked, &repos), url(12));
        let bare = note(
            5,
            "t3:judge",
            NoteKind::Decision,
            "review: pass",
            "2026-11-08T12:00:00Z",
        );
        let with = detail(vec![parking.clone(), bare.clone()], serde_json::json!([]));
        assert_eq!(passed_pr(&with, &bare, &repos), url(11));
        // A link in another agent's note is not the task's pull request.
        let mut others = parking.clone();
        others.agent = "t3:other".parse().unwrap();
        let with_others = detail(vec![others, bare.clone()], serde_json::json!([]));
        assert_eq!(passed_pr(&with_others, &bare, &repos), PassedPr::None);
        // One in a repository the board does not list is not merged.
        let elsewhere = ["github.com/o/other".to_owned()];
        assert!(matches!(
            passed_pr(&with, &bare, &elsewhere),
            PassedPr::Unclear(_)
        ));
        // A `pr:` line that is no URL falls back to the notes, and without one is unclear,
        // not "no pull request".
        let short = note(
            5,
            "t3:judge",
            NoteKind::Decision,
            "review: pass\npr: #12",
            "2026-11-08T12:00:00Z",
        );
        let with_short = detail(vec![parking.clone(), short.clone()], serde_json::json!([]));
        assert_eq!(passed_pr(&with_short, &short, &repos), url(11));
        let alone = detail(vec![short.clone()], serde_json::json!([]));
        assert!(matches!(
            passed_pr(&alone, &short, &repos),
            PassedPr::Unclear(_)
        ));
        assert_eq!(passed_summary(&bare.text), "The review passed.");
        // `pr: none` says the task has no pull request, whatever other notes link.
        let none = note(
            5,
            "t3:judge",
            NoteKind::Decision,
            "review: pass. A report.\npr: none",
            "2026-11-08T12:00:00Z",
        );
        let with = detail(vec![parking, none.clone()], serde_json::json!([]));
        assert_eq!(passed_pr(&with, &none, &repos), PassedPr::None);
    }

    #[test]
    fn a_long_summary_is_cut_to_the_servers_limit() {
        let text = format!("review: pass. {}", "a".repeat(SUMMARY_CHARS_MAX + 50));
        let summary = passed_summary(&text);
        assert_eq!(summary.chars().count(), SUMMARY_CHARS_MAX);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn noted_pr_picks_the_newest_url_in_the_boards_repositories() {
        let repos = ["github.com/o/r".to_owned()];
        let own = note(
            1,
            "t3:worker",
            NoteKind::Finding,
            "PR https://github.com/o/r/pull/38 opened",
            "2026-11-13T08:37:00Z",
        );
        let short = note(
            2,
            "t3:worker",
            NoteKind::Decision,
            "PR o/r#38 squash-merged as 22b65998f3",
            "2026-11-13T09:05:00Z",
        );
        let other = note(
            3,
            "t3:worker",
            NoteKind::Note,
            "related https://github.com/o/other/pull/99",
            "2026-11-13T09:10:00Z",
        );
        let d = detail(vec![own, short, other], serde_json::json!([]));
        assert_eq!(
            noted_pr(&d, &repos).as_deref(),
            Some("https://github.com/o/r/pull/38")
        );
        assert_eq!(
            noted_pr(&d, &[]).as_deref(),
            Some("https://github.com/o/other/pull/99")
        );

        // A note naming both the board's PR and a later PR in another repo still picks the
        // board's PR.
        let mixed = note(
            4,
            "t3:worker",
            NoteKind::Note,
            "PR https://github.com/o/r/pull/41 opened; related https://github.com/o/other/pull/99",
            "2026-11-13T09:15:00Z",
        );
        let d = detail(vec![mixed], serde_json::json!([]));
        assert_eq!(
            noted_pr(&d, &repos).as_deref(),
            Some("https://github.com/o/r/pull/41")
        );
    }

    #[test]
    fn noted_worker_branch_picks_the_newest_dispatcher_start_note() {
        let first = note(
            1,
            "dispatch:mac",
            NoteKind::Note,
            "note: the dispatcher started a worker for this task on branch task/factory-7: https://t3.example.test/threads/1",
            "2026-11-13T08:00:00Z",
        );
        let second = note(
            2,
            "dispatch:mac",
            NoteKind::Note,
            "note: the dispatcher started a worker for this task on branch task/factory-7-20261113-083000: https://t3.example.test/threads/2",
            "2026-11-13T08:30:00Z",
        );
        let spoofed = note(
            3,
            "t3:worker",
            NoteKind::Note,
            "note: the dispatcher started a worker for this task on branch task/fake: https://t3.example.test/threads/3",
            "2026-11-13T08:35:00Z",
        );
        let d = detail(
            vec![first.clone(), second.clone(), spoofed],
            serde_json::json!([]),
        );
        assert_eq!(
            noted_worker_branch(&d),
            Some(Some("task/factory-7-20261113-083000".to_owned()))
        );

        // An in-place dispatcher start stops scanning so an older branch is not reused.
        let in_place = note(
            4,
            "dispatch:mac",
            NoteKind::Note,
            "note: the dispatcher started a worker for this task: https://t3.example.test/threads/4",
            "2026-11-13T09:00:00Z",
        );
        let d = detail(vec![first, second, in_place], serde_json::json!([]));
        assert_eq!(noted_worker_branch(&d), Some(None));

        // Without any dispatcher start note: None.
        let d = detail(vec![], serde_json::json!([]));
        assert_eq!(noted_worker_branch(&d), None);
    }

    #[test]
    fn noted_pr_none_matches_current_round_and_stops_at_reopen_or_new_worker() {
        let repos = ["github.com/o/r".to_owned()];
        let start1 = note(
            1,
            "dispatch:mac",
            NoteKind::Note,
            "note: the dispatcher started a worker for this task on branch task/factory-7: https://t3.example.test/threads/1",
            "2026-11-13T08:00:00Z",
        );
        let none_line = note(
            2,
            "t3:worker",
            NoteKind::Note,
            "Done.\npr: none",
            "2026-11-13T08:10:00Z",
        );
        assert!(noted_pr_none(
            &detail(
                vec![start1.clone(), none_line.clone()],
                serde_json::json!([])
            ),
            &repos
        ));

        let bullet_none = note(
            2,
            "t3:worker",
            NoteKind::Note,
            "Superseded by https://github.com/o/r/pull/40\n- pr: none.",
            "2026-11-13T08:10:00Z",
        );
        assert!(noted_pr_none(
            &detail(vec![bullet_none], serde_json::json!([])),
            &repos
        ));

        // Prose mentioning `pr: none` inline does not trigger.
        for prose in [
            "Fixed pr: none parsing; PR pending.",
            "Report written (not pr: none).",
            "expr: none and pr: none_yet",
        ] {
            let n = note(
                2,
                "t3:worker",
                NoteKind::Note,
                prose,
                "2026-11-13T08:10:00Z",
            );
            assert!(
                !noted_pr_none(&detail(vec![n], serde_json::json!([])), &repos),
                "{prose}"
            );
        }

        // A newer note in the same round naming a same-repo PR overrides an earlier pr: none.
        let later_pr = note(
            3,
            "t3:worker",
            NoteKind::Note,
            "Opened PR https://github.com/o/r/pull/42 after all",
            "2026-11-13T08:15:00Z",
        );
        assert!(!noted_pr_none(
            &detail(
                vec![start1.clone(), none_line.clone(), later_pr],
                serde_json::json!([])
            ),
            &repos
        ));

        // A later review decision (fail, void, or pass) or a newer worker start clears an
        // older round's pr: none.
        let fail = note(
            3,
            "t3:judge",
            NoteKind::Decision,
            "review: fail: needs a code fix",
            "2026-11-13T08:20:00Z",
        );
        let pass = note(
            3,
            "t3:judge",
            NoteKind::Decision,
            "review: pass. Earlier round.\npr: none",
            "2026-11-13T08:20:00Z",
        );
        let start2 = note(
            4,
            "dispatch:mac",
            NoteKind::Note,
            "note: the dispatcher started a worker for this task on branch task/factory-7-20261113-082500: https://t3.example.test/threads/2",
            "2026-11-13T08:25:00Z",
        );
        assert!(!noted_pr_none(
            &detail(
                vec![start1.clone(), none_line.clone(), fail],
                serde_json::json!([])
            ),
            &repos
        ));
        assert!(!noted_pr_none(
            &detail(vec![start1.clone(), pass], serde_json::json!([])),
            &repos
        ));
        assert!(!noted_pr_none(
            &detail(vec![start1, none_line, start2], serde_json::json!([])),
            &repos
        ));
    }

    #[test]
    fn parked_in_review_at_and_within_pr_wait_respect_bounds() {
        let d = detail(vec![], serde_json::json!([parked("2026-11-08T11:30:00Z")]));
        let t: Timestamp = "2026-11-08T11:30:00Z".parse().unwrap();
        assert_eq!(parked_in_review_at(&d), t);
        assert!(within_pr_wait(t, t));
        assert!(within_pr_wait(
            t,
            t + REVIEW_PR_WAIT - TimeDelta::seconds(1)
        ));
        assert!(!within_pr_wait(t, t + REVIEW_PR_WAIT));
        assert!(within_pr_wait(t, t - TimeDelta::minutes(1)));
        assert!(!within_pr_wait(t, t - TimeDelta::minutes(10)));

        // Falls back to task.updated_at when history lacks a review transition.
        let empty = detail(vec![], serde_json::json!([]));
        assert_eq!(parked_in_review_at(&empty), empty.task.updated_at);
    }

    #[test]
    fn after_merge_checks_parses_tags_and_strips_from_summary() {
        let text = "review: pass. Added the lock.\n\
                    check: cargo test --workspace => 0\n\
                    - after merge: deploy/upgrade.sh --yes and verify /healthz\n\
                    * After-Merge: reinstall board on home devices\n\
                    after merge: none.\n\
                    after merge: n/a\n\
                    after merge:   \n\
                    pr: https://github.com/o/r/pull/42";
        assert_eq!(
            after_merge_checks(text),
            vec![
                "deploy/upgrade.sh --yes and verify /healthz".to_owned(),
                "reinstall board on home devices".to_owned(),
            ]
        );
        assert_eq!(passed_summary(text), "Added the lock.");
    }

    #[test]
    fn after_merge_handoff_roundtrips_and_combines_pre_and_post_merge_checks() {
        let me: AgentId = "dispatch:mac".parse().unwrap();
        let before = serde_json::json!([parked("2026-11-08T11:00:00Z")]);
        let start1 = note(
            1,
            "dispatch:mac",
            NoteKind::Note,
            &format!("{JUDGE_NOTE} thread j1 (agent t3:judge1)"),
            "2026-11-08T11:05:00Z",
        );
        let pre_pass = note(
            2,
            "t3:judge1",
            NoteKind::Decision,
            "review: pass. Code verified.\ncheck: cargo test --workspace => 0\nafter merge: deploy/upgrade.sh --yes\npr: https://github.com/o/r/pull/42",
            "2026-11-08T11:10:00Z",
        );
        // Before the dispatcher writes AFTER_MERGE_READY_NOTE: Reviewed::Passed (to merge first).
        let d_pre = detail(vec![start1.clone(), pre_pass.clone()], before.clone());
        assert!(matches!(reviewed(&d_pre, &me), Reviewed::Passed(n) if n.id == 2));

        let pr = PullRequest {
            url: "https://github.com/o/r/pull/42".to_owned(),
            state: PrState::Merged,
            draft: false,
            base_ref: "main".to_owned(),
            head_ref: "task/factory-7".to_owned(),
            head_oid: "111111111111".to_owned(),
            cross_repository: false,
            merge_commit: Some("deadbeef1234".to_owned()),
            merge_state: MergeState::Clean,
            checks: Checks::Passed,
            passed_checks: Vec::new(),
            review_decision: ReviewDecision::None,
        };
        let ready = note(
            3,
            "dispatch:mac",
            NoteKind::Note,
            &format_after_merge_ready_note(
                pre_pass.id,
                Some(&pr),
                &after_merge_checks(&pre_pass.text),
            ),
            "2026-11-08T11:15:00Z",
        );
        // After AFTER_MERGE_READY_NOTE: Reviewed::NeedsAfterMerge.
        let d_wait = detail(
            vec![start1.clone(), pre_pass.clone(), ready.clone()],
            before.clone(),
        );
        assert!(matches!(reviewed(&d_wait, &me), Reviewed::NeedsAfterMerge(n) if n.id == 2));
        let info = after_merge_ready_info(&d_wait, pre_pass.id).unwrap();
        assert_eq!(
            info.pr_url.as_deref(),
            Some("https://github.com/o/r/pull/42")
        );
        assert_eq!(info.merge_commit.as_deref(), Some("deadbeef1234"));
        assert_eq!(info.base_ref.as_deref(), Some("main"));

        // After-merge judge starts and writes a clean pass without `after merge:` lines.
        let start2 = note(
            4,
            "dispatch:mac",
            NoteKind::Note,
            &format!("{JUDGE_NOTE} thread j2 (after-merge) (agent t3:judge2)"),
            "2026-11-08T11:16:00Z",
        );
        let post_pass = note(
            5,
            "t3:judge2",
            NoteKind::Decision,
            "review: pass. Deployed and verified.\ncheck: deploy/upgrade.sh --yes deadbeef1234 => 0\npr: https://github.com/o/r/pull/42",
            "2026-11-08T11:20:00Z",
        );
        let d_post = detail(
            vec![start1, pre_pass, ready, start2, post_pass.clone()],
            before,
        );
        assert!(matches!(reviewed(&d_post, &me), Reviewed::Passed(n) if n.id == 5));
        let checks = collect_pass_checks(&d_post, &post_pass);
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert_eq!(checks[0].value, "cargo test --workspace");
        assert_eq!(checks[1].value, "deploy/upgrade.sh --yes deadbeef1234");
    }

    #[test]
    fn passed_pr_prefers_after_merge_handoff_over_stale_owner_notes_and_loop_guard_catches_bypasses()
     {
        let repos = ["github.com/o/r".to_owned()];
        let stale_owner_note = note(
            1,
            "t3:worker",
            NoteKind::Note,
            "Earlier attempt: https://github.com/o/r/pull/10",
            "2026-11-08T11:01:00Z",
        );
        // Case A: handoff recorded PR #20; after-merge pass omits `pr:` -> resolves PR #20,
        // NOT stale owner PR #10.
        let ready_pr20 = note(
            2,
            "dispatch:mac",
            NoteKind::Note,
            "note: merged; waiting for after-merge checks (pass #1):\npr: https://github.com/o/r/pull/20\nbase: main",
            "2026-11-08T11:10:00Z",
        );
        let post_omitted_pr = note(
            3,
            "t3:judge2",
            NoteKind::Decision,
            "review: pass. Deployed.",
            "2026-11-08T11:15:00Z",
        );
        let d_pr20 = detail(
            vec![
                stale_owner_note.clone(),
                ready_pr20.clone(),
                post_omitted_pr.clone(),
            ],
            serde_json::json!([]),
        );
        assert_eq!(
            passed_pr(&d_pr20, &post_omitted_pr, &repos),
            PassedPr::Url("https://github.com/o/r/pull/20".to_owned())
        );

        // Case B: handoff recorded `pr: none`; after-merge pass omits `pr:` -> resolves None,
        // NOT stale owner PR #10.
        let ready_none = note(
            2,
            "dispatch:mac",
            NoteKind::Note,
            "note: merged; waiting for after-merge checks (pass #1):\npr: none",
            "2026-11-08T11:10:00Z",
        );
        let d_none = detail(
            vec![stale_owner_note, ready_none, post_omitted_pr.clone()],
            serde_json::json!([]),
        );
        assert_eq!(passed_pr(&d_none, &post_omitted_pr, &repos), PassedPr::None);

        // Case C: loop guard catches same PR, `pr: none` after PR #20, and repeated PRs,
        // while allowing one brand-new follow-up PR #21.
        assert!(after_merge_loop_detected(
            &d_pr20,
            post_omitted_pr.id,
            Some("https://github.com/o/r/pull/20")
        ));
        assert!(after_merge_loop_detected(&d_pr20, post_omitted_pr.id, None));
        assert!(!after_merge_loop_detected(
            &d_pr20,
            post_omitted_pr.id,
            Some("https://github.com/o/r/pull/21")
        ));
        // Two prior handoffs in the same round cap any further follow-up handoff.
        let ready_pr21 = note(
            4,
            "dispatch:mac",
            NoteKind::Note,
            "note: merged; waiting for after-merge checks (pass #3):\npr: https://github.com/o/r/pull/21\nbase: main",
            "2026-11-08T11:20:00Z",
        );
        let d_two = detail(vec![ready_pr20, ready_pr21], serde_json::json!([]));
        assert!(after_merge_loop_detected(
            &d_two,
            5,
            Some("https://github.com/o/r/pull/22")
        ));
    }

    #[test]
    fn merge_hold_tracks_per_head_resets_on_change_and_deserializes_legacy_json() {
        let t0: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        // Legacy state without `hold` deserializes cleanly.
        let mut merge: Merge = serde_json::from_value(serde_json::json!({
            "read_at": t0,
            "failed_head": "aaa111",
        }))
        .unwrap();
        assert_eq!(merge.hold, None);

        assert!(!merge.track_hold(MergeHoldKind::FailedChecks, "aaa111", t0));
        assert!(!merge.track_hold(
            MergeHoldKind::FailedChecks,
            "aaa111",
            t0 + PR_HOLD_WAIT - TimeDelta::seconds(1),
        ));
        assert!(merge.track_hold(MergeHoldKind::FailedChecks, "aaa111", t0 + PR_HOLD_WAIT));

        // Pushing a new head resets the hold timer.
        assert!(!merge.track_hold(MergeHoldKind::FailedChecks, "bbb222", t0 + PR_HOLD_WAIT));
        assert!(!merge.track_hold(
            MergeHoldKind::FailedChecks,
            "bbb222",
            t0 + PR_HOLD_WAIT + TimeDelta::minutes(29),
        ));
        assert!(merge.track_hold(
            MergeHoldKind::FailedChecks,
            "bbb222",
            t0 + PR_HOLD_WAIT + PR_HOLD_WAIT,
        ));

        // Clearing the hold (e.g. a check rerun goes Pending) starts fresh next time.
        merge.clear_hold();
        assert_eq!(merge.hold, None);
        assert!(!merge.track_hold(
            MergeHoldKind::FailedChecks,
            "bbb222",
            t0 + TimeDelta::hours(2),
        ));

        // Backward clock jump resets `since` rather than stalling forever.
        assert!(!merge.track_hold(
            MergeHoldKind::FailedChecks,
            "bbb222",
            t0 + TimeDelta::hours(1),
        ));
        assert_eq!(merge.hold.as_ref().unwrap().since, t0 + TimeDelta::hours(1));

        // Entering a newer review round resets stale failed_head and hold.
        merge.read_at = t0 + TimeDelta::hours(1);
        merge.reset_if_before(t0 + TimeDelta::hours(2));
        assert_eq!(merge.failed_head, None);
        assert_eq!(merge.hold, None);

        assert!(answer_requests_reopen("Reopen for a worker"));
        assert!(answer_requests_reopen("  reopen "));
        assert!(!answer_requests_reopen("Approved — resume merge"));
    }

    #[test]
    fn dispatch_blocked_to_review_keeps_standing_pass_while_doing_or_other_invalidates() {
        let me: AgentId = "dispatch:mac".parse().unwrap();
        let started = note(
            1,
            "dispatch:mac",
            NoteKind::Note,
            &format!("{JUDGE_NOTE} thread j (agent t3:judge)"),
            "2026-11-08T11:30:00Z",
        );
        let pass = note(
            2,
            "t3:judge",
            NoteKind::Decision,
            "review: pass",
            "2026-11-08T12:00:00Z",
        );
        let dispatch_resume = serde_json::json!([
            parked("2026-11-08T11:00:00Z"),
            {
                "seq": 10, "at": "2026-11-08T12:30:00Z", "board": "factory",
                "actor": "dispatch:mac", "type": "task_updated", "task": "factory-7",
                "payload": {"from": "review", "to": "blocked"},
            },
            {
                "seq": 11, "at": "2026-11-08T12:40:00Z", "board": "factory",
                "actor": "dispatch:mac", "type": "task_updated", "task": "factory-7",
                "payload": {"from": "blocked", "to": "review"},
            },
        ]);
        let d = detail(vec![started.clone(), pass.clone()], dispatch_resume);
        assert!(matches!(reviewed(&d, &me), Reviewed::Passed(n) if n.id == 2));

        // Non-dispatch blocked -> review invalidates the standing pass.
        let worker_resume = serde_json::json!([
            parked("2026-11-08T11:00:00Z"),
            {
                "seq": 11, "at": "2026-11-08T12:40:00Z", "board": "factory",
                "actor": "t3:worker", "type": "task_updated", "task": "factory-7",
                "payload": {"from": "blocked", "to": "review"},
            },
        ]);
        let d = detail(vec![started.clone(), pass.clone()], worker_resume);
        assert_eq!(reviewed(&d, &me), Reviewed::Waits);

        // Moving to doing after pass invalidates it even if dispatch later moved blocked -> review.
        let via_doing = serde_json::json!([
            parked("2026-11-08T11:00:00Z"),
            {
                "seq": 10, "at": "2026-11-08T12:20:00Z", "board": "factory",
                "actor": "t3:worker", "type": "task_updated", "task": "factory-7",
                "payload": {"from": "review", "to": "doing"},
            },
            {
                "seq": 11, "at": "2026-11-08T12:40:00Z", "board": "factory",
                "actor": "dispatch:mac", "type": "task_updated", "task": "factory-7",
                "payload": {"from": "blocked", "to": "review"},
            },
        ]);
        let d = detail(vec![started, pass], via_doing);
        assert_eq!(reviewed(&d, &me), Reviewed::Waits);
    }
}
