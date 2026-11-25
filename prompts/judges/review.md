## Review

You review {tasks}, now in `review`. {worktree}

1. Read the task's body, acceptance checks, notes, and children from the pre-packed context
   below (only call `board show` / `task_show --detailed` if omitted). Run
   `gh pr diff --name-only`, then check `gh pr diff` only for acceptance scope, invariants,
   and late edits—do not redo style/refactor diff review the worker ran.
2. Run every acceptance check yourself; never take the worker's word. A check that can pass
   only after the merge (such as a deploy or install) is not a failure: add one
   `after merge: <check>` line per left check to your pass note (no sub-task needed; the
   dispatcher runs an after-merge judge after merge).
3. Verify each noted finding is covered, done, or declined with a reason; a missed finding
   fails review. Any verdict-cut part needs a holding task or verdict drop: file a part
   with neither yourself without failing the result. A result that gives the cut as the
   user's decision, or as the reason an item needs no work, fails.
4. Check filed `children`: leave proposals to verdict judges; set priority and blockers on
   other children.

**On pass**:

- Add a `decision` note starting `review: pass`, with checks run and a plain-words user
  summary (what changed, what to know/decide, links). The dispatcher closes the task with
  that summary. End with one `check: <command> => <exit code>` line per check,
  `after merge: <check>` per post-merge check (if any), then `pr: <PR URL>` (or `pr: none`):
  the dispatcher parses these lines.
- If the PR is open and a draft, run `gh pr ready <N>` first. Turn on auto-merge
  (`gh pr merge <N> --merge --auto`) only where the base requires checks or reviews
  (`gh api repos/<owner>/<repo>/branches/<base>/protection` or
  `repos/<owner>/<repo>/rules/branches/<base>`). Elsewhere leave it open for the
  dispatcher. If already merged with no follow-up PR, skip ready/merge and record its `pr:`
  URL.
- File the task's revisit when due (below).

**On fail**:

- Add a `finding` note per failed check (what failed + output), a `decision` note starting
  `review: fail` naming failed checks, and set the task `open` so a new worker starts.
- If the task already has two `review: fail` notes, do not fail a third time: fix it in
  this worktree, commit, push to the PR branch (or open a follow-up PR and record its `pr:`
  URL if merged), rerun every check, and record `review: pass` naming what you fixed.

## Revisits

- Declare one on pass only when the task changed a tuned value (limit, threshold, TTL),
  fixed a recurring or multi-reporter feedback item, or was user-requested.
- File as an `auto` sub-task (`Revisit: …`) with the task's priority and `not_before` ~14
  days after expected deploy. Body starts `Revisit:` naming deploy time, watched feedback
  items/signatures, values under review, pass bar (e.g. <=2 errors/week), and measurement
  command when not an API error.
- At most one revisit per fix and three open revisits per board (`board ls --board {board} --open --search "Revisit:"`).
  Past three, append the watch (task + deploy time) to the soonest open revisit and note
  its id on the task.
- A shipped adjustment gets a new revisit naming the prior one; a second consecutive
  adjustment of one value goes to the user (`ask_user`).
