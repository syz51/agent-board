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
    DoneTask, EvidenceKind, Judge, JudgeKind, NewEvidence, Note, RegisterJudge, SUMMARY_CHARS_MAX,
    TaskDetail, UpdateTask,
};

use super::*;
use crate::gh::{
    Checks, MergeState, PrState, PullRequest, Updated, pull_request_url, pull_request_url_in,
    repo_of,
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

/// How long the dispatcher waits for a task in `review` without a recorded PR or pushed
/// worker branch on `origin` before falling back to the repository's default base.
pub const REVIEW_PR_WAIT: TimeDelta = TimeDelta::minutes(2);

/// A judge thread the dispatcher started, until its judge ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeRun {
    pub board: BoardSlug,
    pub kind: JudgeKind,
    /// The tasks it judges; none for a goal review.
    #[serde(default)]
    pub tasks: Vec<TaskId>,
    /// Its thread or runner-native session id; `None` until `Runner::spawn` returns, as for
    /// a [`Worker`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// Runner kind that owns this judge (`"t3"` when omitted in older state files).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
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

/// What the dispatcher knows of the pull request of a task it closes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Merge {
    /// When it last read the pull request.
    pub read_at: Timestamp,
    /// The head commit whose failed checks it noted on the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_head: Option<String>,
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

/// Extra context needed to choose the worktree base and brief for a review judge.
#[derive(Debug, Clone)]
struct ReviewWant {
    /// The pull request URL recorded on the task, if any.
    pr: Option<String>,
    /// Whether the current review attempt's notes explicitly declare `pr: none`.
    no_code: bool,
    /// The branch the dispatcher gave the worker (`task/<id>` or its time-suffixed form),
    /// tried on `origin` when no PR is recorded yet.
    worker_branch: Option<String>,
    /// When the task last moved to `review`.
    parked_at: Timestamp,
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
    /// A judge passed it, and nothing changed since: the dispatcher closes it.
    Passed(&'a Note),
    /// The dispatcher withdrew its pass and has not set it `open` yet.
    Withdrawn,
    /// It waits for a review judge.
    Waits,
}

/// The start of the note the dispatcher adds for the review judge it started.
pub(crate) const JUDGE_NOTE: &str = "note: the dispatcher started a review judge for this task:";

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
    let parked_again = detail.history.iter().any(|e| {
        e.event_type == EventType::TaskUpdated && e.payload["to"] == "review" && e.at > note.at
    });
    if parked_again {
        return Reviewed::Waits;
    }
    let judge = format!("(agent {})", note.agent);
    let by_judge = detail.task.owner.as_ref() != Some(&note.agent)
        && detail.notes.iter().any(|n| {
            n.agent == *dispatcher && n.text.starts_with(JUDGE_NOTE) && n.text.ends_with(&judge)
        });
    match verdict {
        ReviewVerdict::Pass if by_judge => Reviewed::Passed(note),
        ReviewVerdict::Void => Reviewed::Withdrawn,
        ReviewVerdict::Pass | ReviewVerdict::Fail => Reviewed::Waits,
    }
}

/// A `<name>: <value>` line of a pass note, as `prompts/judges/review.md` asks for them.
fn tagged<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let line = line.trim().trim_start_matches(['-', '*']).trim_start();
    Some(line.strip_prefix(name)?.strip_prefix(':')?.trim())
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
        .filter(|l| tagged(l, "check").is_none() && tagged(l, "pr").is_none())
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
                    if let Err(err) = self
                        .close_reviewed(board, agent, &client, &project, &detail, pass, now)
                        .await
                    {
                        tracing::warn!("closing {}: {err:#}", task.id);
                    }
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
                            no_code,
                            worker_branch,
                            parked_at: parked_in_review_at(&detail),
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
            && self.state.goal_due.contains(slug)
            && in_progress == 0
            && !work_left
            && in_review.is_empty()
            && wants.is_empty()
            && at_work.count == 0
            && !self.state.judge_retry.contains_key(&goal_key(slug))
        {
            wants.push(Want {
                kind: JudgeKind::Goal,
                tasks: Vec::new(),
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
        if run.tasks.is_empty() {
            let retry = JudgeRetry {
                until,
                updated_at: None,
            };
            self.state.judge_retry.insert(goal_key(&run.board), retry);
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
                .insert(retry_key(run.kind, task), JudgeRetry { until, updated_at });
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
                brief_kind(run.kind),
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
        let judge = RunHandle::new(runner_kind, &thread).resolve_agent_id()?;
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
                let text = format!("{JUDGE_NOTE} {link} (agent {judge})");
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
                let judge = r
                    .thread
                    .as_deref()
                    .and_then(|t| RunHandle::new(runner_kind, t).resolve_agent_id().ok());
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
        let (worktree, worktree_note) = match (want.kind, want.tasks.first(), &want.review) {
            (JudgeKind::Review, Some(task), Some(review)) => {
                use crate::worktree::{ReviewBase, ReviewTarget};
                let (pr_ref, fallback_reason) = if let Some(pr_url) = &review.pr {
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
                } else if review.no_code {
                    (None, Some("task notes declare pr: none".to_owned()))
                } else {
                    (None, None)
                };
                let target = if let Some(branch) = pr_ref.as_deref() {
                    ReviewTarget::PullRequest(branch)
                } else if review.pr.is_none() && !review.no_code {
                    match review.worker_branch.as_deref() {
                        Some(branch) => ReviewTarget::TaskBranch(branch),
                        None => ReviewTarget::None,
                    }
                } else {
                    ReviewTarget::None
                };
                let waiting_without_target = review.pr.is_none()
                    && !review.no_code
                    && matches!(target, ReviewTarget::None)
                    && within_pr_wait(review.parked_at, now);
                if waiting_without_target
                    && crate::worktree::is_repo_top(project)
                        .await
                        .context("checking the review judge's repository")?
                {
                    let until = review.parked_at + REVIEW_PR_WAIT;
                    let cause = format!(
                        "{task} has no PR recorded yet and no worker branch; waiting until {until} ({}s from park) before starting on the default base",
                        REVIEW_PR_WAIT.num_seconds()
                    );
                    self.record_hold("review_wait", &board.slug, &want.title, 0, &cause, now);
                    tracing::info!("review of {task}: {cause}");
                    return Ok(StartOutcome::Waited);
                }
                match crate::worktree::review_worktree(project, task, target, now)
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
                            if review.pr.is_none()
                                && !review.no_code
                                && within_pr_wait(review.parked_at, now)
                            {
                                let until = review.parked_at + REVIEW_PR_WAIT;
                                let branch_desc = review
                                    .worker_branch
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
                                review.no_code,
                            );
                            (Some(prepared.worktree), Some(note))
                        }
                    },
                }
            }
            _ => (None, None),
        };
        let brief = crate::brief::judge_brief_with_worktree(
            brief_kind(want.kind),
            &board.slug,
            &want.tasks,
            worktree_note.as_deref(),
        );
        let run_label = want
            .tasks
            .first()
            .map_or_else(|| board.slug.to_string(), ToString::to_string);
        let run_id = format!(
            "judge-{}-{run_label}-{}",
            brief_kind(want.kind).as_str(),
            now.format("%Y%m%d-%H%M%S")
        );
        let cost_units = device::cost_units_for_judge(want.kind);
        let intended_branch = worktree.as_ref().map(|w| w.branch.clone());

        // Write-before-act: commit `local_runs` intent and judge record into `daemon.db`
        // BEFORE creating any git worktree or spawning the runner (agent-board-539).
        let mut run = JudgeRun {
            board: board.slug.clone(),
            kind: want.kind,
            tasks: want.tasks,
            thread: None,
            runner: Some(self.runner.kind().to_owned()),
            worktree_id: None,
            worktree_path: None,
            run_id: Some(run_id.clone()),
            title: want.title,
            started_at: now,
            registered: false,
            noted: false,
            worktree_note,
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
        let spec = RunSpec {
            run_id: run_id.clone(),
            title: run.title.clone(),
            prompt: brief,
            model,
            worktree: spec_worktree,
            metadata: BTreeMap::from([
                ("board".to_owned(), board.slug.to_string()),
                ("kind".to_owned(), format!("judge:{}", run.kind_name())),
            ]),
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
        if self.runner.kind() == "t3" {
            thread_agent(&thread)
                .with_context(|| format!("t3 started thread {thread:?}; archive it in T3"))?;
        } else {
            handle.resolve_agent_id()?;
        }
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
    /// be merged, then finishes the task once its worker no longer runs.
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
    ) -> Result<()> {
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
                return Ok(());
            }
        }
        let mut merged = None;
        let passed = passed_pr(detail, pass, &board.repos);
        if passed != PassedPr::None {
            let mut merge = match self.state.merges.get(&task.id) {
                // A clock set back reads at once.
                Some(merge)
                    if (TimeDelta::zero()..PR_READ_EVERY).contains(&(now - merge.read_at)) =>
                {
                    return Ok(());
                }
                Some(merge) => merge.clone(),
                None => Merge {
                    read_at: now,
                    failed_head: None,
                },
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
                    return Ok(());
                }
                PassedPr::None => unreachable!("checked above"),
            };
            let pr = self
                .gh
                .view(project, &url)
                .await
                .with_context(|| format!("reading {url}"))?;
            match self
                .advance(agent, client, project, task, &url, pr, &mut merge)
                .await?
            {
                Some(pr) => merged = Some(pr),
                None => return Ok(()),
            }
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
        Ok(())
    }

    /// Moves the pull request of a passed task toward its merge, and returns it once it is
    /// merged. A pull request that conflicts or was closed sends the task back to `open`.
    #[allow(clippy::too_many_arguments)]
    async fn advance(
        &mut self,
        agent: &AgentId,
        client: &Client,
        project: &Path,
        task: &Task,
        url: &str,
        pr: PullRequest,
        merge: &mut Merge,
    ) -> Result<Option<PullRequest>> {
        match pr.state {
            PrState::Merged => return Ok(Some(pr)),
            PrState::Closed => {
                let why = format!("its pull request {url} was closed without a merge");
                self.reopen(agent, client, task, &why).await?;
                return Ok(None);
            }
            PrState::Open => {}
        }
        let conflict = format!(
            "its pull request {url} conflicts with its base branch. Resolve the conflict on the pull request's branch `{}`",
            pr.head_ref
        );
        match (pr.merge_state, &pr.checks) {
            (MergeState::Conflict, _) => self.reopen(agent, client, task, &conflict).await?,
            (MergeState::Behind, _) => match self.gh.update_branch(project, url).await? {
                Updated::Done => {
                    tracing::info!("{}: {url} was behind its base; updated its branch", task.id);
                }
                Updated::Conflict => self.reopen(agent, client, task, &conflict).await?,
            },
            (_, Checks::Failed(names)) => {
                if merge.failed_head.as_deref() != Some(pr.head_oid.as_str()) {
                    let text = clip_note(format!(
                        "finding: the checks {} of {url} failed at commit {}. The dispatcher merges it once every check passes.",
                        names.join(", "),
                        pr.head_oid.chars().take(12).collect::<String>()
                    ));
                    add_note(client, &task.id, agent, NoteKind::Finding, text).await?;
                    merge.failed_head = Some(pr.head_oid.clone());
                    self.state.merges.insert(task.id.clone(), merge.clone());
                }
            }
            (MergeState::Clean, Checks::Passed) => {
                self.gh
                    .merge(project, url, &pr.head_oid)
                    .await
                    .with_context(|| format!("merging {url}"))?;
                tracing::info!("{}: merged {url}", task.id);
                let after = self.gh.view(project, url).await?;
                if after.state == PrState::Merged {
                    return Ok(Some(after));
                }
            }
            // Checks still run, or GitHub holds the merge: read again later.
            (MergeState::Clean | MergeState::Waits, Checks::Pending | Checks::Passed) => {}
        }
        Ok(None)
    }

    /// Withdraws the pass of `task` and sets it `open`, so a new worker starts.
    async fn reopen(
        &mut self,
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
        self.state.merges.remove(&task.id);
        tracing::info!("{}: set open: {why}", task.id);
        Ok(())
    }
}

impl JudgeRun {
    fn kind_name(&self) -> &'static str {
        brief_kind(self.kind).as_str()
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
            Reviewed::Passed(note) => Some(Some(note.id)),
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
}
