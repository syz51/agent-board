//! The briefs of daemon dispatch (`docs/DESIGN.md`, "Daemon dispatch"): the worker brief
//! a task's thread starts with, and the judge briefs, the shared judge rules followed by
//! one section per kind. `board brief <slug> --kind <kind>` prints each one filled in.

use board_proto::{BoardSlug, TaskId};

/// The brief of a worker thread, which does one task.
pub const WORKER_PROMPT: &str = include_str!("../../../prompts/worker.md");
/// The rules every judge brief starts with.
pub const JUDGE_SHARED_PROMPT: &str = include_str!("../../../prompts/judges/shared.md");
/// The review judge's section, after [`JUDGE_SHARED_PROMPT`].
pub const REVIEW_PROMPT: &str = include_str!("../../../prompts/judges/review.md");
/// The verdict judge's section, after [`JUDGE_SHARED_PROMPT`].
pub const VERDICT_PROMPT: &str = include_str!("../../../prompts/judges/verdict.md");
/// The goal judge's section, after [`JUDGE_SHARED_PROMPT`].
pub const GOAL_PROMPT: &str = include_str!("../../../prompts/judges/goal.md");
/// The triage judge's section, after [`JUDGE_SHARED_PROMPT`].
pub const TRIAGE_JUDGE_PROMPT: &str = include_str!("../../../prompts/judges/triage.md");

/// Who reviews a worker's result on a board set to daemon dispatch.
pub const WORKER_CLOSER: &str = "The board's review judge";

/// The most characters a filled judge brief may have, so each judge starts small.
pub const JUDGE_BRIEF_CHARS_MAX: usize = 6_000;

/// The judgment a judge thread is started for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeKind {
    /// A task moved to `review`.
    Review,
    /// Pending proposals need verdicts.
    Verdict,
    /// The board's goal review is due.
    Goal,
    /// Feedback items are ready on the feedback board.
    Triage,
}

impl JudgeKind {
    pub const ALL: [Self; 4] = [Self::Review, Self::Verdict, Self::Goal, Self::Triage];

    /// The kind as the server names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Verdict => "verdict",
            Self::Goal => "goal",
            Self::Triage => "triage",
        }
    }

    /// Whether the brief names tasks: every kind but the goal review does.
    pub fn takes_tasks(self) -> bool {
        self != Self::Goal
    }

    fn section(self) -> &'static str {
        match self {
            Self::Review => REVIEW_PROMPT,
            Self::Verdict => VERDICT_PROMPT,
            Self::Goal => GOAL_PROMPT,
            Self::Triage => TRIAGE_JUDGE_PROMPT,
        }
    }
}

/// The brief of the worker that does `task` on `board`; `closer` names who reviews its
/// result, as the start of a sentence, such as [`WORKER_CLOSER`].
pub fn worker_brief(board: &BoardSlug, task: &TaskId, closer: &str) -> String {
    WORKER_PROMPT
        .replace("{board}", board.as_str())
        .replace("{task}", task.as_str())
        .replace("{closer}", closer)
}

/// The default worktree sentence in a review judge's brief.
const REVIEW_PR_WORKTREE: &str = "You run in a worktree on the head of its pull request, or on its base branch when the pull request is already merged.";

/// The worktree sentence for a review judge started on the worker's task branch fetched
/// from `origin` when no pull request was recorded yet.
pub fn review_task_branch_note(base: &str) -> String {
    format!(
        "You run in a worktree on `{base}`, the head of the task's branch fetched from origin (no pull request was recorded yet when this review started)."
    )
}

/// The worktree sentence for a review judge whose worktree started on the repository's
/// default base because no pull request head or task branch resolved (or because the task
/// declared `pr: none`).
pub fn review_default_base_note(base: &str, no_code: bool) -> String {
    if no_code {
        format!(
            "You run in a worktree on `{base}` (the repository's default base), because the task's notes declare `pr: none` (no pull request)."
        )
    } else {
        format!(
            "You run in a worktree on `{base}` (default base), not the PR head: no PR or task branch resolved at start. Check the notes and fetch the PR or branch before checking code, unless no code changed."
        )
    }
}

/// The brief of a `kind` judge of `board`, for `tasks`: the shared rules, then the kind's
/// section. A goal judge names no tasks.
pub fn judge_brief(kind: JudgeKind, board: &BoardSlug, tasks: &[TaskId]) -> String {
    judge_brief_with_worktree(kind, board, tasks, None)
}

/// Like [`judge_brief`], with an optional worktree sentence replacing `{worktree}` in a
/// review judge's brief.
pub fn judge_brief_with_worktree(
    kind: JudgeKind,
    board: &BoardSlug,
    tasks: &[TaskId],
    worktree: Option<&str>,
) -> String {
    let worktree = worktree.unwrap_or(REVIEW_PR_WORKTREE);
    format!("{JUDGE_SHARED_PROMPT}\n{}", kind.section())
        .replace("{board}", board.as_str())
        .replace("{kind}", kind.as_str())
        .replace("{tasks}", &list(tasks))
        .replace("{worktree}", worktree)
}

/// `a`, `a and b`, `a, b and c`.
fn list(tasks: &[TaskId]) -> String {
    match tasks {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => {
            let rest: Vec<&str> = rest.iter().map(TaskId::as_str).collect();
            format!("{} and {last}", rest.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> BoardSlug {
        "factory".parse().unwrap()
    }

    fn task(n: u64) -> TaskId {
        TaskId::new(&board(), n)
    }

    #[test]
    fn every_brief_is_filled_in() {
        let worker = worker_brief(&board(), &task(7), WORKER_CLOSER);
        assert!(!worker.contains('{'), "{worker}");
        assert!(worker.contains("`task_claim factory-7`"), "{worker}");
        assert!(
            worker.contains("The board's review judge reviews"),
            "{worker}"
        );
        for kind in JudgeKind::ALL {
            let tasks = if kind.takes_tasks() {
                vec![task(7)]
            } else {
                vec![]
            };
            let brief = judge_brief(kind, &board(), &tasks);
            assert!(!brief.contains('{'), "{kind:?}: {brief}");
            assert!(brief.starts_with("# Judge\n"), "{kind:?}");
            assert!(
                brief.contains(&format!(
                    "You are the {} judge of board factory.",
                    kind.as_str()
                )),
                "{kind:?}"
            );
            if kind.takes_tasks() {
                assert!(brief.contains("factory-7"), "{kind:?}");
            }
        }
    }

    /// A verdict judge's cut is not the user's decision, and no brief lets it pass as one.
    #[test]
    fn a_cut_is_accounted_for_and_never_the_users_decision() {
        let verdict = judge_brief(JudgeKind::Verdict, &board(), &[task(7)]);
        for rule in [
            "A cut is your call, never the user's decision.",
            "Scope the user asked for is not yours to cut",
            "`cut: <part> -> <task id>` or `cut: <part>, dropped: <reason>`",
            "You never drop it: only the user does.",
            "`Out of scope (cut by a judge): <part>, <task id>`, with `dropped` in place of the id",
        ] {
            assert!(flat(&verdict).contains(rule), "verdict: {rule}");
        }
        let worker = worker_brief(&board(), &task(7), WORKER_CLOSER);
        assert!(
            flat(&worker).contains("never give the cut as the reason that a limit or an open item"),
            "{worker}"
        );
        // A part the verdict dropped is not filed again.
        assert!(
            flat(&worker)
                .contains("or the verdict's drop and its reason. A part with neither: file it"),
            "{worker}"
        );
        let review = judge_brief(JudgeKind::Review, &board(), &[task(7)]);
        assert!(
            flat(&review).contains("A result that gives the cut as the user's decision")
                && flat(&review).contains("file a part with neither yourself"),
            "{review}"
        );
    }

    #[test]
    fn worker_opens_draft_pr_and_review_marks_ready_on_pass() {
        let worker = flat(&worker_brief(&board(), &task(7), WORKER_CLOSER));
        assert!(worker.contains("gh pr create --draft"), "{worker}");
        assert!(
            worker.contains("gh pr merge <N> --disable-auto"),
            "{worker}"
        );
        assert!(
            worker.contains("Do not turn on auto-merge and do not merge it"),
            "{worker}"
        );

        let review = flat(&judge_brief(JudgeKind::Review, &board(), &[task(7)]));
        let ready_at = review
            .find("gh pr ready <N>")
            .expect("review brief must mark draft PRs ready on pass");
        let auto_merge_at = review
            .find("gh pr merge <N> --merge --auto")
            .expect("review brief must mention auto-merge on pass");
        assert!(
            ready_at < auto_merge_at,
            "gh pr ready must precede auto-merge in review brief: {review}"
        );
    }

    #[test]
    fn worker_proceeds_on_disjoint_lease_overlap_and_stops_otherwise() {
        let worker = flat(&worker_brief(&board(), &task(7), WORKER_CLOSER));
        for rule in [
            "read the other task's branch or local worktree diff of the overlapping files before stopping",
            "touches only different sections or hunks than the ones factory-7 will change",
            "record a `decision` note on factory-7 naming the other task, each overlapping file and the disjoint sections or hunks",
            "add a note on the other task",
            "merge `origin/main` (or the base branch) before finishing",
            "When both tasks change the same sections or hunks, or when the other branch's diff cannot be read",
            "stop: set factory-7 `open` with a note naming that task, and end.",
        ] {
            assert!(worker.contains(rule), "missing {rule:?} in {worker}");
        }
    }

    /// `text` with each run of white space as one space, so a rule matches across lines.
    fn flat(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn each_judge_brief_stays_under_its_size() {
        let tasks: Vec<TaskId> = (1000..1005).map(task).collect();
        for kind in JudgeKind::ALL {
            let chars = judge_brief(kind, &board(), &tasks).chars().count();
            assert!(
                chars < JUDGE_BRIEF_CHARS_MAX,
                "{kind:?} brief has {chars} characters"
            );
        }
        for note in [
            review_task_branch_note("origin/task/factory-1000-20261108-132005"),
            review_default_base_note("origin/main", false),
            review_default_base_note("origin/main", true),
        ] {
            let brief =
                judge_brief_with_worktree(JudgeKind::Review, &board(), &[task(1000)], Some(&note));
            assert!(!brief.contains('{'), "{brief}");
            assert!(
                brief.chars().count() < JUDGE_BRIEF_CHARS_MAX,
                "custom review brief has {} characters",
                brief.chars().count()
            );
            assert!(
                !flat(&brief).contains("on the head of its pull request"),
                "{brief}"
            );
        }
    }

    #[test]
    fn tasks_read_as_a_list() {
        assert_eq!(list(&[task(1)]), "factory-1");
        assert_eq!(list(&[task(1), task(2)]), "factory-1 and factory-2");
        assert_eq!(
            list(&[task(1), task(2), task(3)]),
            "factory-1, factory-2 and factory-3"
        );
    }
}
