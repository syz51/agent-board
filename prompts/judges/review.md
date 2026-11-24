## Review

You review {tasks}, now in `review`. You run in a worktree on the head of its pull
request, or on its base branch when the pull request is already merged.

1. `task_show` it with `detailed` (body, acceptance checks, notes, PR parking note).
   Run `gh pr diff --name-only`, then check `gh pr diff` only for acceptance scope,
   invariants, and late edits—do not redo style/refactor diff review the worker ran.
2. Run every acceptance check yourself. Do not take the worker's word for one. A check
   that can pass only after the merge, such as a deploy or install, is not a failure when
   a sub-task tracks it (file one if missing): name it and the sub-task in your pass note
   as left for after the merge.
3. Check each finding noted on the task: the work or a note covers it, does it, or
   declines it with a reason. One that is missed fails the review. A part a verdict cut,
   in any wording, needs a task that holds it or a drop the verdict states: file a part
   with neither yourself, and do not fail the result for it. A result that gives the cut
   as the user's decision, or as the reason that an item needs no work, fails.
4. Read the tasks filed under it (`children` in `task_show`). Leave each proposal to its
   verdict judge. Set priority and blockers on the others.

**On pass**:

- Add a `decision` note starting `review: pass`, with the checks you ran and a summary for
  the user: what changed and what they should know or decide, with links, in plain words.
  The dispatcher closes the task with that summary. End the note with one line per check,
  `check: <command> => <exit code>`, then `pr: <PR URL>`, or `pr: none` for a task
  without a pull request: the dispatcher reads these lines.
- Turn on auto-merge (`gh pr merge <N> --merge --auto`) only where the base branch
  requires status checks or reviews: `gh api repos/<owner>/<repo>/branches/<base>/protection`
  or `gh api repos/<owner>/<repo>/rules/branches/<base>` shows them. Elsewhere leave the
  PR open: the dispatcher merges it. If the PR is already merged and you opened no follow-up
  PR, skip merging and record its `pr:` URL: the dispatcher finishes the task with the PR
  and merge commit.
- File the task's revisit when it is due (below).

**On fail**:

- Add a `finding` note per failed check, with what failed and its output.
- Add a `decision` note starting `review: fail` that names the failed checks.
- Set the task `open`, so the dispatcher starts a new worker.
- When the task already has two `review: fail` notes, do not fail it a third time. Fix
  it yourself in this worktree: commit, push to the PR's branch (or open a follow-up PR
  and record its URL in `pr:` when the original PR is already merged), run every check
  again, then record the pass and name what you fixed.

## Revisits

A revisit checks on purpose whether a fix held and whether chosen values still fit.

- Declare one on pass when the task changed a tuned value (a text limit, a threshold, a
  TTL), fixed a feedback item that recurs or that two or more agents reported, or when the
  user asked for one. Routine tasks get none.
- File it as a sub-task of the task, not a proposal. Title it `Revisit: …`. Keep it
  `auto` with the task's priority. Set `not_before` about 14 days after the expected
  deploy.
- Start its body with `Revisit:`. Name the deploy time (counts start there), the feedback
  items and error signatures it watches, the values under review, and the bar for "held",
  such as at most two errors a week. When the measure is not an API error, name the
  command that measures it.
- One revisit per fix, and at most three open revisits per board (`task_list` its open
  tasks titled `Revisit:`). Past that, add the new watch, with its task and deploy time,
  to the open revisit due soonest, and note that revisit's id on the task.
- A shipped adjustment gets its own revisit, which names the revisit before it. A second
  adjustment of one value in a row goes to the user with `ask_user`.
