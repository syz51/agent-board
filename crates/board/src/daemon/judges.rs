//! The dispatcher's judges, merges and finishes on a daemon-dispatch board
//! (`docs/DESIGN.md`, "Daemon dispatch", Judges, Merging and finishing).
//!
//! Each reconcile reads what the board needs judged, and starts a judge thread for each
//! need while fewer than [`JUDGES_AT_ONCE`] run:
//! - a task in `review` with no standing `review: pass` gets a review judge, in a worktree
//!   on the head of its pull request;
//! - pending proposals get a verdict judge per parent task, at most
//!   [`JUDGE_TASKS_AT_ONCE`] proposals each;
//! - ready items on the feedback board get a triage judge;
//! - a board with a goal gets a goal judge once a task was finished and nothing is ready,
//!   in progress, in review or being judged.
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
use crate::gh::{Checks, MergeState, PrState, PullRequest, Updated, pull_request_url, repo_of};

/// The most judges of one board at work at once; the rest wait, the oldest need first.
pub const JUDGES_AT_ONCE: usize = 2;

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

/// A judge thread the dispatcher started, until its judge ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeRun {
    pub board: BoardSlug,
    pub kind: JudgeKind,
    /// The tasks it judges; none for a goal review.
    #[serde(default)]
    pub tasks: Vec<TaskId>,
    /// Its thread; `None` until `t3 thread start` returns, as for a [`Worker`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub title: String,
    pub started_at: Timestamp,
    /// Whether the server has it as a judge of the board.
    #[serde(default)]
    pub registered: bool,
    /// Whether its thread is noted on the task it reviews.
    #[serde(default)]
    pub noted: bool,
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

/// A judgment the board needs.
#[derive(Debug, Clone)]
struct Want {
    kind: JudgeKind,
    tasks: Vec<TaskId>,
    title: String,
    /// Since when it has waited: the oldest goes first.
    since: Timestamp,
    /// The pull request of a task to review.
    pr: Option<String>,
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
enum ReviewVerdict {
    Pass,
    Fail,
    /// The dispatcher withdrew a pass: the pull request could not be merged.
    Void,
}

/// The verdict a note records: its text starts `review: pass`, `review: fail` or
/// `review: void`, after the note's kind when the author wrote it first.
fn review_verdict(text: &str) -> Option<ReviewVerdict> {
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
const JUDGE_NOTE: &str = "note: the dispatcher started a review judge for this task:";

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
enum PassedPr {
    Url(String),
    /// The task has none.
    None,
    /// The notes name one the dispatcher must not merge, or cannot read: why.
    Unclear(String),
}

/// The pull request the review passed: the pass note's `pr:` line, else the last pull
/// request it links, else the one in the task owner's newest note before it. `pr: none`
/// is none. It must be in one of the board's `repos`, when the board lists any.
fn passed_pr(detail: &TaskDetail, pass: &Note, repos: &[String]) -> PassedPr {
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

/// The pull request the newest note of a task names.
fn noted_pr(detail: &TaskDetail) -> Option<String> {
    detail
        .notes
        .iter()
        .rev()
        .find_map(|n| pull_request_url(&n.text))
}

/// The checks a pass note records, one `check: <command> => <exit code>` line each.
fn passed_checks(pass: &str) -> Vec<NewEvidence> {
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
fn passed_summary(pass: &str) -> String {
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

impl<T: T3> Daemon<T> {
    /// Follows the judges started on `board`, closes its reviewed tasks, and starts the
    /// judges it needs. `in_progress` is how many tasks hold a slot.
    pub(super) async fn judge_and_close(
        &mut self,
        board: &Board,
        agent: &AgentId,
        in_progress: usize,
        now: Timestamp,
    ) -> Result<()> {
        let slug = &board.slug;
        let project = self.host_folder(board)?;
        let client = self.client().with_agent(Some(agent.clone()));
        self.settle_judge_starts(board, &client, &project, now)
            .await;
        let listed = client
            .judges(slug)
            .await
            .context("listing the board's judges")?;
        self.end_judges(slug, &client, &listed, now).await;
        self.state.judge_retry.retain(|_, retry| retry.until > now);
        let at_work = AtWork::of(&listed, &self.state.judges, slug);
        let mut wants = Vec::new();

        let in_review: Vec<Task> = client
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
            .collect();
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
                    wants.push(Want {
                        kind: JudgeKind::Review,
                        tasks: vec![task.id.clone()],
                        title: format!("review {} {}", task.id, task.title),
                        since: task.updated_at,
                        pr: noted_pr(&detail),
                    });
                }
            }
        }

        let proposals = client
            .tasks(&TaskQuery {
                board: Some(slug.clone()),
                proposal: Some(true),
                limit: Some(board_proto::TASKS_LIMIT_MAX),
                ..TaskQuery::default()
            })
            .await
            .context("listing the pending proposals")?;
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
                pr: None,
            });
        }

        let ready = client
            .tasks(&TaskQuery {
                board: Some(slug.clone()),
                ready: Some(true),
                proposal: Some(false),
                limit: Some(board_proto::TASKS_LIMIT_MAX),
                ..TaskQuery::default()
            })
            .await
            .context("listing the ready tasks")?;
        if self.triages(slug) {
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
                    pr: None,
                });
            }
        }

        // The goal review comes last: once a task was finished and the board is at rest.
        let work_left = ready
            .iter()
            .any(|t| t.ready && t.auto && t.feedback.is_none());
        if !board.goal.trim().is_empty()
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
                pr: None,
            });
        }

        // A board at its usage stop gets no new judge, as it gets no new worker.
        if self.usage_holds(slug, now) {
            return self.save();
        }
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
            let names = want.title.clone();
            match self.start_judge(board, &client, &project, want, now).await {
                Ok(()) => started += 1,
                Err(err) => tracing::warn!("starting the judge `{names}`: {err:#}"),
            }
        }
        self.save()
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

    /// Forgets `run`, and keeps its tasks from another judge of its kind for `wait`
    /// unless they change.
    async fn end_run(&mut self, run: &JudgeRun, client: &Client, now: Timestamp, wait: TimeDelta) {
        self.state.judges.retain(|r| !r.same(run));
        let until = now + wait;
        if run.tasks.is_empty() {
            let retry = JudgeRetry {
                until,
                updated_at: None,
            };
            self.state.judge_retry.insert(goal_key(&run.board), retry);
        }
        for task in &run.tasks {
            let updated_at = match client.task(task).await {
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
                    Ok(Some(thread)) => run.thread = Some(thread),
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
            let brief = crate::brief::judge_brief(brief_kind(run.kind), &board.slug, &run.tasks);
            if let Err(err) = self.t3.send(&found.id, &brief).await {
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
        if !run.registered {
            client
                .register_judge(
                    &run.board,
                    &RegisterJudge {
                        agent: thread_agent(&thread)?,
                        kind: run.kind,
                        tasks: run.tasks.clone(),
                    },
                )
                .await?;
            run.registered = true;
        }
        if run.kind == JudgeKind::Review && !run.noted {
            let link = match self.t3.thread_link(&thread).await {
                Ok(Some(link)) => link,
                _ => format!("thread {thread}"),
            };
            let Some(agent) = client.agent() else {
                return Ok(());
            };
            // `reviewed` reads the judge's agent id at the end of this note.
            let judge = thread_agent(&thread)?;
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
                let judge = r.thread.as_deref().and_then(|t| thread_agent(t).ok());
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
        project: &Path,
        want: Want,
        now: Timestamp,
    ) -> Result<()> {
        let worktree = match (want.kind, want.tasks.first()) {
            (JudgeKind::Review, Some(task)) => {
                let head = match &want.pr {
                    Some(pr) => match self.gh.view(project, pr).await {
                        // A fork's branch is not on `origin`.
                        Ok(pr) => (!pr.cross_repository).then_some(pr.head_ref),
                        Err(err) => {
                            tracing::warn!("reading {pr} for the review of {task}: {err:#}");
                            None
                        }
                    },
                    None => None,
                };
                crate::worktree::review_worktree(project, task, head.as_deref(), now)
                    .await
                    .context("preparing the review judge's worktree")?
            }
            _ => None,
        };
        let brief = crate::brief::judge_brief(brief_kind(want.kind), &board.slug, &want.tasks);
        let mut run = JudgeRun {
            board: board.slug.clone(),
            kind: want.kind,
            tasks: want.tasks,
            thread: None,
            title: want.title,
            started_at: now,
            registered: false,
            noted: false,
        };
        // On disk before T3 can create the thread.
        self.put_run(&run);
        self.save()?;
        let thread = self
            .t3
            .start_thread(project, &run.title, &brief, worktree.as_ref())
            .await?;
        thread_agent(&thread)
            .with_context(|| format!("t3 started thread {thread:?}; archive it in T3"))?;
        tracing::info!(
            "started {} judge thread {thread} for {} on {}",
            run.kind_name(),
            run.names(),
            board.slug
        );
        run.thread = Some(thread);
        if run.kind == JudgeKind::Goal {
            self.state.goal_due.remove(&board.slug);
        }
        self.put_run(&run);
        self.save()?;
        // Tried again on the next reconcile.
        if let Err(err) = self.register_run(client, &mut run).await {
            tracing::warn!(
                "registering the {} judge of {}: {err:#}",
                run.kind_name(),
                run.names()
            );
        }
        self.put_run(&run);
        Ok(())
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
                        owner: Some(agent.clone()),
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
}
