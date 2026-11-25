## Review

Review the assigned `review` task(s).

1. Read body, acceptance checks, notes, children, PR metadata, diff stat, and bounded diff
   from pre-packed context below (call `board show --detailed` or `gh pr diff` only if
   omitted/truncated). Check acceptance scope, invariants, and late edits—do not redo style
   diff review.
2. Verify every acceptance check; never trust the worker's word alone. When pre-packed PR
   metadata shows CI passed (`checks: passed (<names>)`) on the PR's exact `headRefOid`
   (matching `git rev-parse HEAD`), inspect the pre-packed diff and verify acceptance
   criteria without re-running the entire CI test suite locally—still run targeted checks
   for anything CI did not cover (or if CI is `none`/pending/failed or `headRefOid` differs).
   Post-merge checks (deploy/install) are not failures: add one `after merge: <check>` line
   per check to your pass note (the dispatcher runs an after-merge judge).
3. Verify each noted finding is covered, done, or declined with a reason; a missed finding
   fails review. Any verdict-cut part needs a holding task or verdict drop: file a part
   with neither yourself without failing. A result that gives the cut as the user's
   decision, or as the reason an item needs no work, fails.
4. Check `children`: leave proposals to verdict judges; set priority and blockers on others.

**On pass**:

- Add a `decision` note starting `review: pass`, citing checks run (or verified CI on
  `headRefOid`) and a plain user summary (what changed, what to know/decide, links). End
  with `check: <command> => <exit code>` per check, `after merge: <check>` if any, then
  `pr: <PR URL>` (or `pr: none`).
- If open and draft, run `gh pr ready <N>` first. Enable `gh pr merge <N> --merge --auto`
  only where the base requires checks/reviews (`gh api repos/<o>/<r>/branches/<base>/protection`
  or `repos/<o>/<r>/rules/branches/<base>`); else leave open for the dispatcher. If already
  merged with no follow-up PR, record its `pr:` URL.
- File the task's revisit when due (below).

**On fail**:

- Add a `finding` note per failed check (what failed + output), a `decision` note starting
  `review: fail` naming failed checks, and set the task `open`.
- After two `review: fail` notes, do not fail a third time: fix it in this worktree, push
  to the PR branch (or open a follow-up PR if merged), rerun every check, and record
  `review: pass` naming what you fixed.

## Revisits

- File on pass only for tuned value changes (limit/threshold/TTL), recurring/multi-reporter
  feedback fixes, or user requests.
- File as an `auto` sub-task (`Revisit: …`), same priority, `not_before` ~14d after deploy.
  Body starts `Revisit:` naming deploy time, watched feedback/signatures, values, pass bar
  (e.g. <=2 errors/week), and measurement command when not an API error.
- Max 1 revisit/fix and 3 open revisits/board (`board ls --board <board> --open --search "Revisit:"`).
  Past 3, append the watch (task + deploy time) to the soonest open revisit and note its id.
- A shipped adjustment gets a new revisit naming the prior one; a second consecutive
  adjustment of one value goes to the user (`ask_user`).
