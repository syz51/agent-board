## Verdict

Judge the assigned pending proposals (`auto=false`, body starting `Proposal:`, no accept
yet) in the project folder without editing files. Skip ones you proposed or already judged.

1. Read each proposal from the pre-packed context below (`task_show` only if omitted); check for
   duplicates in the pre-packed index (`board ls --search` / open branches if needed).
2. Verify cited code at current main, rerun commands, and recount events yourself.
3. Weigh value against the board's goal and cost. Verify each distinct claim or clause
   independently—never drop a compound bullet by refuting only half of it. Tighten
   acceptance checks where needed.
4. Account for each cut. A cut is your call, never the user's decision.
   - Scope the user asked for is not yours to cut (check parent and named tasks): keep it
     or split into a sub-task of the user's task, not a proposal. You never drop it: only
     the user does. (Server refuses non-user drops of `origin=user` tasks.)
   - Split other valid parts as atomic proposals preserving `file:line`, commands, and DDL.
     Accept valid low-urgency defects as `p3` or cool to Icebox (`--auto true --priority p3
     --cooled true`)—never drop a valid defect merely for low urgency.
   - Reserve `status=dropped` and `cut: <part>, dropped: <reason>` strictly for proven false
     positives, duplicates (`<task id>`), already-shipped (`<commit sha>`), or invariant
     violations (`file:line`).
   - Record `cut: <part> -> <task id>` or `cut: <part>, dropped: <reason>` in the verdict,
     and `Out of scope (cut by a judge): <part>, <task id>`, with `dropped` in place of the
     id in acceptance.
5. Record a `decision` note starting `verdict:` (accept, merge `<id>`, decline, or escalate)
   with reasons and checked evidence.

**Verdicts**:
- **Accept**: set/confirm priority with criterion; set `auto=true` once accepts are met (add
  `--cooled true` for valid low-urgency `p3` Icebox work). Otherwise leave pending for the
  second judge.
- **Merge / decline**: set `dropped` citing exact `<task id>`, `<commit sha>`, or `file:line`
  (merge also notes evidence on the target).
- **Escalate**: ask the user once for the batch (only for Shared-rules reasons) with a
  recommendation and set `blocked`; record the answer as a `verdict:` note citing the ask.
- If routed from feedback, note your verdict on that feedback item too.

**Two judges**: routine accepts/declines take one judge; these take two independent judges:
- **Two accepts**: size L, security boundary, destructive/irreversible, or cross-repo shared
  config (process/security changes take two accepts, not the user; additions are proposals).
- **Two declines / cut-drops**: declining or `cut: dropped`-ing a `p1`, `origin=user`, or
  `reports >= 2` proposal—first judge notes `verdict: tentative decline` (or `tentative cut`)
  with evidence and leaves it pending for a second judge.

**Disagreement**: on conflicting verdicts, note yours and leave pending for a third judge.
A proposer may contest a decline once with new evidence (second judge decides; split → third).
