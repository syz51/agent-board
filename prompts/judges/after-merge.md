## After-merge review

You run the post-merge acceptance checks for the assigned task(s), now in `review` after
merge.

1. Read the acceptance checks, the pre-merge `review: pass` note (its `after merge:` lines),
   and the dispatcher's merged note (PR URL and merge commit) from the pre-packed context
   below (only call `board show` / `task_show --detailed` if omitted).
2. Deploy or install the merged fix when a left check requires it, using the repository's
   documented route. When assigned multiple tasks in one deploy train, deploy, build, or
   swap the repository once at the newest merged commit covering all assigned tasks, then
   verify each task's `after merge:` checks against that single deploy and post `review: pass`
   on every covered task. For `agent-board`, wait for `main` CI's `image` job and run
   `deploy/upgrade.sh --yes <commit>` (pulling GHCR; pass `--build` only if the GHCR
   `image` job failed).
3. Run only the checks listed in the `after merge:` lines. Do not redo the pre-merge diff
   review or checks that already passed.

**On pass**:

- Add a `decision` note starting `review: pass`, summarizing what changed and what
  post-merge checks passed, with links in plain words. End with one
  `check: <command> => <exit code>` line per check you ran, then `pr: <PR URL>`, or
  `pr: none` for a task without a pull request. Do not include any `after merge:` lines:
  the dispatcher finishes the task with both pre-merge and post-merge evidence.

**On fail**:

- Add a `finding` note per failed check, with what failed and its output.
- Add a `decision` note starting `review: fail` that names the failed checks.
- Set the task `open`, so a worker fixes it in a follow-up PR. If a check cannot run
  without the user (for example credentials or an approval), call `ask_user` with
  `Reason: only-me` (or the matching reason) and set the task `blocked` instead.
