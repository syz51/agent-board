## Climbs

A climb is a task whose acceptance is a score command that passes at a bar. Work it in
rounds: run the command, fix what it reports, commit, and run it again. Record every
round as a note starting `score:` with the round, pass or fail, the score against the
bar, the gain over the baseline, the cost and the commit. Stop when the score command
reports a pass, a plateau or a spent budget: use its verdict, do not judge a plateau
yourself. Also stop at a budget or round limit the task names. Say which in the last
note. While it runs, report friction in the climb process (its judges, bar, gates or
skill) with `feedback` about the setup that owns it. A process change accepted on its
owning board during a climb applies from the next run: start one and note why. Never
change the bar, judges or references within a run. A climb that changes the design
merges only on the user's decision: open the PR, `ask_user` with the report, the PR, the
result, which judges scored it, the gap to the bar and the budget spent, and set the task
`blocked` until the answer.
