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

/// The brief of a `kind` judge of `board`, for `tasks`: the shared rules, then the kind's
/// section. A goal judge names no tasks.
pub fn judge_brief(kind: JudgeKind, board: &BoardSlug, tasks: &[TaskId]) -> String {
    format!("{JUDGE_SHARED_PROMPT}\n{}", kind.section())
        .replace("{board}", board.as_str())
        .replace("{kind}", kind.as_str())
        .replace("{tasks}", &list(tasks))
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
