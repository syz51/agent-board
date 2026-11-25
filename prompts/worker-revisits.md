## Revisits

A task whose title starts `Revisit:` checks whether a shipped fix held.

1. Count the errors with `board errors --since <deploy time>` for each watch its body
   names. Records older than the server's `error_retention_days` (35 by default) are gone:
   say so when the window is cut.
2. Read the feedback items routed to it and their reports.
3. Decide `held` when under the bar, or `adjust` when over the bar or the values no longer
   fit. For `adjust`, file a proposal with the counts and the change, parent the revisit.
   For a value the user chose, `ask_user` instead, with the counts and a recommendation.
4. Add a `decision` note starting `revisit: held` or `revisit: adjust`, with the counts,
   the window and the bar. Add it to each fixed task it watches and to each watched
   feedback item, so their reporters see the answer.
5. Park the revisit in `review` with a note that gives the counts.
