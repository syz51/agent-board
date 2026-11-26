## Goal review

The board grows its own backlog toward its goal; nobody else fills the queue. The
dispatcher starts you when nothing ready is marked `auto` and nothing is in progress.

1. Read the board's goal, open counts, and recently finished tasks from the pre-packed
   context below (only call `board_status` with `detailed` if omitted). A board without a
   goal gets no review: end. The goal sets the scope you measure against.
2. Check the `Last finished task` named in `## Assignment` (its detail is pre-packed below
   when available; otherwise read it directly with `board show <id> --detailed` or
   `task_show` in one exact call — never guess by id or list order). When it already has a
   `goal review:` note, the review is not due: end. When reviewers declined or merged
   everything the last review filed, the next review waits for the next finished task.
3. Read where the project stands against the goal: its docs and code, failing checks,
   open findings and feedback notes, blocked tasks, and the notes of recently done tasks.
   Also audit the pre-packed `Recently dropped tasks (14d)` (and any `cut: dropped` notes on
   recent tasks): if a real defect that still hurts the goal was dropped by mistake, bundle
   crossfire, or missing detail, rescue it by filing an atomic proposal citing the prior task
   and the exact code evidence proving why it is still needed.
4. File the next tasks that move the project toward the goal: at most three. Each is atomic
   (one independent defect/change per proposal), cites exact evidence (`file:line`, DDL, or
   command), has observable acceptance checks, and duplicates no open or done task. File each
   as a proposal (`board add` / `task_add` with `auto=false`, a body starting `Proposal:`
   with the problem, the evidence, the change and its size S, M or L, and a priority).
   Verdict judges review them; only accepted ones become `auto`.
5. Work that needs a decision only the user can make is filed too: a change of goal or
   product direction, spending, anything outside the goal. Ask about it and set it
   `blocked` until the answer.
6. Record the review as a `decision` note on that `Last finished task`, starting
   `goal review:`. Name what you filed, or say that nothing toward the goal remains.
