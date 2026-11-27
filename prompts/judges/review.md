## Review

1. Read task, notes, children, PR metadata, diff stat, and bounded diff from pre-packed
   context below (`board show --detailed` or `gh pr diff` only if truncated). Check
   acceptance, invariants, and late edits—skip style diff review.
2. Verify every acceptance check; never trust the worker alone. If pre-packed PR metadata
   shows `checks: passed (<names>)` on exact `headRefOid` (`git rev-parse HEAD`), inspect
   the diff and verify acceptance without re-running the entire CI test suite locally—run
   targeted checks for anything uncovered (or if CI is `none`/pending/failed or
   `headRefOid` differs). Post-merge checks (deploy/install) are not failures: add
   `after merge: <check>` per check (siblings batch into a deploy train). For standard
   `agent-board` deploys with no extra checks, write `after merge: deploy/upgrade.sh --yes`
   for the zero-LLM deploy train. Never emit `after merge:` for work already delegated to
   a blocked child task (deadlocks parent/child—pass so it unblocks) or disruptive actions
   requiring quitting/restarting T3 Code while unrelated threads are active (track
   separately).
3. Each noted finding must be covered, done, or declined with a reason; a missed one fails
   review. Verdict-cut parts need a holding task or drop: file a part with neither yourself
   without failing; contest a mistaken drop with an atomic proposal + proof. A result that
   gives the cut as the user's decision, or as the reason an item needs no work, fails.
4. Check `children`: leave proposals to verdict judges; set priority and blockers on others.

**On pass**:

- Add a `decision` note starting `review: pass` citing checks (or CI on `headRefOid`) and
  a plain user summary (what changed, what to know/decide, links). End with
  `check: <command> => <exit code>` per check, `after merge: <check>` if any, then
  `pr: <PR URL>` (or `pr: none`).
- If open draft, run `gh pr ready <N>` first. Enable `gh pr merge <N> --merge --auto`
  only if base requires checks/reviews (`gh api repos/<o>/<r>/branches/<base>/protection`
  or `repos/<o>/<r>/rules/branches/<base>`); else leave open for the dispatcher. If merged
  with no follow-up PR, record its `pr:` URL.
- File a revisit when due (below).

**On fail**:

- Add a `finding` note per failed check (what + output), a `decision` note starting
  `review: fail` naming them, and set task `open`.
- After two `review: fail` notes, fix it here instead of failing again: push to PR branch
  (or open a follow-up PR if merged), rerun checks, and post `review: pass` naming your fix.

## Revisits

- File on pass only for tuned values (limit/threshold/TTL), recurring/multi-reporter
  feedback fixes, or user requests.
- Create an `auto` sub-task (`Revisit: …`), same priority, `not_before` ~14d post-deploy.
  Body starts `Revisit:` with deploy time, watched feedback/signatures, values, pass bar
  (e.g. <=2 errors/week), and measurement cmd if not an API error.
- Max 1/fix and 3 open/board (`board ls --board <board> --open --search "Revisit:"`); past
  3, append watch (task + deploy time) to the soonest open revisit and note its id.
- Shipped adjustments get a new revisit naming the prior one; a second consecutive
  adjustment of one value goes to the user (`ask_user`).
