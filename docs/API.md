# HTTP API, version 1

The server's HTTP API is the only boundary: the CLI, the MCP server, the hooks, the device
daemon and the web view all use it. Bodies are JSON; their types are in
`crates/board-proto` (named in brackets below), with `snake_case` fields and enum values and
RFC 3339 UTC timestamps.

## Compatibility

Within `/v1`, changes are additive, because clients run for hours (`board mcp`, the daemon)
and are upgraded separately from the server:

- New endpoints, and new request fields that are optional.
- New response fields: clients ignore fields they do not know, and a field a client expects
  but an older server lacks gets a default (`#[serde(default)]`).
- New values of the open enums `ResolutionKind` (`Feedback.resolution.kind`) and
  `EventType` (`Event.type`): clients read a value they do not know as `unknown`
  (`#[serde(other)]` in `board-proto`; `oneOfOrUnknown` or a plain string in the web view),
  so the item or event still reads and renders. The server never stores `unknown` and
  refuses it in a request (400 `invalid`); a server rolled back past a kind it stored reads
  that kind as `unknown` too. `ResolutionKind` became open in the same release that added
  `fixed`, so clients from before that release fail to read an item resolved `fixed`:
  restart `board mcp` processes and daemons after upgrading.

Every other enum is closed: a new value would fail every read that carries it in a client
that predates it. Add one only after making the enum open as above and waiting until running
clients have been restarted on a version that reads it.

## Authentication

| Caller | How | May call |
|---|---|---|
| Device (CLI, MCP server, hooks, daemon) | `Authorization: Bearer <device token>` | everything except "user only" endpoints |
| User in the browser | Identity header set by `tailscale serve` (`Tailscale-User-Login`), matched against the server's `users` setting | everything, including "user only" |
| User on the CLI | A device token created with `--user` | everything, including "user only" |
| Telegram | Not an HTTP caller: the server long-polls the Bot API and accepts answers only from the user's chat: `[telegram] chat_id`, else the chat paired with `/start <code>` | answering asks |

The server listens on loopback only and is published on the tailnet by `tailscale serve`,
so the identity header can only come from Tailscale. Tokens are random 32-byte values,
shown once, stored as SHA-256 hashes, and revocable.

Agent requests also send `X-Board-Agent: <agent id>`. The device token authenticates the
device; the agent id is asserted by that device's local client, which reads it from the
runtime's environment. Every request body field named `agent`, `RegisterAgent.id` and
every agent id in a path must equal the header. Agent ids in paths are percent-encoded
(`:` as `%3A`, `%` as `%25`).

Every `repo` field (boards' `repos`, agents, leases, touches) takes an origin URL in any
common form (`https://host/owner/repo.git`, `git@host:owner/repo`, `ssh://git@host:22/owner/repo`)
or a key, and is stored and returned as the key `board_proto::normalize_repo` makes:
`host/owner/repo`, lowercased as a whole, without user, port or `.git`. In scp form
(`git@host:12345/repo`) the part after the colon is always the path, never a port. Local
paths and `file://` URLs are `invalid`.

## Errors

Every error is `[ApiError]` `{code, message, next}` with these statuses:

| Status | Codes |
|---|---|
| 400 | `invalid`, `evidence_required`, `invalid_transition` |
| 401 / 403 | `unauthorized`, `forbidden` |
| 404 | `not_found` |
| 409 | `version_conflict`, `claim_refused` |
| 500 | `internal` |

`next` tells an agent what to do, for example `"claim refused: t3:c2d0 owns factory-12
(seen 2m ago); pick another task from ready"`. For text over a limit the message is
`"<field> is <n> characters; the limit is <max>"` and `next` says to shorten it or where
longer text goes, such as splitting a note or moving detail to the task body
(`board_proto::TooLong`).

**Signals.** The server records every error it answers to an authenticated request that
names an agent (`X-Board-Agent`), except expected answers: `version_conflict` (a normal
retry), `not_found` from `GET /v1/agents/{id}` and `GET /v1/agents/{id}/handoff` (clients
probe for them), `claim_refused` from `POST /v1/tasks/{id}/claim` when another agent has the task (a message
starting `claim refused:`; agents race for tasks, and stats count refusals as
`claims_refused`) and `claim_refused` from
`POST /v1/boards/{slug}/coordinator` (the daemon renews it). It records the code, the
route template (`POST /v1/tasks/{id}/claim`) and the message normalized (numbers become
`N`, task ids `ID`, agent ids `AGENT`, quoted text `"…"`). Once the same code, route and template reach
`signal_min_reports` errors from at least `signal_min_agents` agents within
`signal_window_days`, the server files feedback itself (see "Feedback") with source
`signal`, about `board` and signature `signal:<code>:<hash>`, giving the count, the agents,
the window, the route and the template. Later errors add reports to that item while it is
live. Once it is no longer live (its fix is done), only errors after its last report count
towards a new item, which `recurs` it. Records older than the window are deleted by the
server's minute sweep.

## Endpoints

### Health

- `GET /healthz` → `{"ok": true, "seq": <latest event>}`; no authentication.

### Agents

- `POST /v1/agents` `[RegisterAgent]` → `[Agent]`. Idempotent; refreshes `last_seen` and
  replaces repo, branch and cwd (where the agent runs now; one left out is cleared). A
  `title` or `forked_from` left out keeps the stored one, and `native_ids` are merged into
  the stored ones. A new id with `forked_from` starts with no tasks. A device token
  registers agents only for its own `device` (403 otherwise); the user may name any device. An agent named
  as a board's coordinator keeps the `coordinator` role whatever role it registers with.
  Clients register on session start (hooks) and before acting when the agent is unknown.
- `GET /v1/agents?board=&state=` → `[Agent]` list. With `board`, agents that ended more
  than 7 days ago are left out unless `state=ended`; they stay members of the board.
- `GET /v1/agents/{id}` → `[Agent]`.
- `POST /v1/agents/{id}/state` `[AgentStateUpdate]` → `[Agent]`. Heartbeat and lifecycle.
  `ended` releases the agent's leases and coordinator lease; owned tasks stay owned.
  `said`, optional, is the last message of the turn that just ended: the hooks send it
  with the `Stop` update. It is advisory and never fails the update: the server removes
  terminal escape sequences and control characters other than line breaks and tabs, trims
  it, cuts it to 4,000 characters (`SAID_CHARS_MAX`, ending with `…`) and drops it when
  nothing is left (`turn_ending` in board-proto, which the hooks apply first). While the
  agent holds a live coordinator lease it is kept, once per such board, as a `[Said]` for
  the user (see "Updates"), and the update records an `agent_state_changed` event even when
  the state did not change; otherwise it is dropped. The newest 50 per agent and board are
  kept.
- `GET /v1/agents/{id}/handoff` → `[Handoff]` or 404. `last_change_at` is when the agent
  last changed the board or touched a file (its newest event other than a handoff,
  registration or state change), on the same clock as `updated_at`; a handoff older than it
  is stale. It is absent when the agent has made no change.
- `PUT /v1/agents/{id}/handoff` `[PutHandoff]` → `[Handoff]`; 409 on a version mismatch.
- `GET /v1/agents/{id}/said?limit=` → `[Said]` list: the agent's kept turn endings,
  newest first, `limit` 1 to 50 (default 20). User only: agents never read what another
  agent told the user.

### Boards

- `GET /v1/boards` → `[Board]` list. `POST /v1/boards` `[CreateBoard]` → `[Board]`.
- `GET /v1/boards/{slug}` → `[Board]`. `PATCH /v1/boards/{slug}` `[UpdateBoard]` → `[Board]`.
- `POST /v1/boards/{slug}/join` `[JoinBoard]` → `[Board]`.
- `POST /v1/boards/{slug}/coordinator` `[ClaimCoordinator]` → `[Board]`; 409
  `claim_refused` when another device holds the coordinator lease (45 minutes, renewed by
  the daemon and by the coordinator's heartbeats). The device claims first, starts the
  thread, then claims again with the thread's `agent` id, which makes that agent's role
  `coordinator`. `DELETE` releases it; only the holding device or the user may. Both emit
  `board_updated` with `coordinator` and the lease's `device` in the payload.
- `DELETE /v1/boards/{slug}` → 204. User only. Refused with 400 `invalid_transition` unless
  the board is `closed` and has no open asks. Deletes the board with its tasks, blockers,
  notes, evidence, asks, members, refused-claim records and events (including lease events
  that name its tasks); leases that named one of its tasks keep running without it. Tasks
  on other boards that one of its tasks blocked lose that blocker (see "Tasks"). One
  event is then recorded for the board: `board_updated` with `{"fields": ["deleted"],
  "title": …}`, an existing type, so clients that predate deletion still read the feed. The
  slug may be used again, and its task numbers continue where the deleted board's stopped,
  so a task id an agent still holds never names a new task. Ask ids are never reused
  either.
- `GET /v1/stats?days=7` → `[Stats]`. `days` is 1 to 365, default 7.

#### Temporary boards

A board created with `temporary: true` is for a test or a one-off. Agents may set it when
creating a board and anyone may turn it off; turning it on for an existing board is user
only (403 `forbidden` otherwise), because the server then deletes the board. Every minute
the server sweeps temporary boards that are **idle**: no event on the board (nor change to
it) for the period, no open ask, and no task in `doing` whose owner is live. Tasks in
`review` or `blocked` do not keep a board; ask the user to keep one waiting on them. The
period counts from the server's start at the earliest, so time the server was down is not
idle time. Nor is a board idle while one of its tasks that is not done blocks an
unfinished task on another board. Each board is swept in its own transaction; one that
fails is logged and tried again on the next sweep.

- An `active` or `paused` temporary board idle for `temporary_close_after_secs` is closed:
  `board_updated` with `fields: ["status"]` and actor `server`.
- A `closed` temporary board idle for `temporary_delete_after_secs` is deleted as by
  `DELETE` above, with actor `server`. Closing is an event, so deletion comes that long after
  the board was closed at the earliest.

Reopening a closed board, or any change on it, starts the period again.

### Tasks

- `GET /v1/tasks?board=&status=&owner=&ready=&mine=&limit=` → `[Task]` list, sorted by
  priority, then id number. `mine=true` uses the `X-Board-Agent` header. `limit` defaults
  to 200, at most 1,000.
- `POST /v1/tasks` `[CreateTask]` → `[Task]` with status `open`. An optional
  `not_before` (RFC 3339, kept to the millisecond) holds the task: it is not `ready` until
  the server's clock passes it, and a time already passed is ignored. The `task_created`
  event's payload has `title`, `priority` and, when set, `parent`. While the new task is
  neither done nor dropped, an agent with an unfinished task anywhere above it, other than
  its creator, sees it in its snapshot alerts (`<agent> filed <ids> under your task <id>`).
  - `claim: true` creates the task in `doing` owned by `agent`, in the same transaction, so
    it is never `ready` and no coordinator is woken for it: an agent that files work it
    starts at once no longer races the coordinator for it. It is checked as a claim is,
    before anything is written: 409 `claim_refused` at the caller's `doing` limit (the
    error names the task in `doing` and says to move it to `review` or `blocked` first), and
    400 `invalid_transition` while a blocker is not done or `not_before` is ahead. A refused
    request creates nothing and uses no task number. The `task_created` payload then also
    has `"claimed": true`, and a `task_claimed` event (`previous_owner: null`) follows.
  - Compatibility: `claim` is sent only when true, so older clients' requests are
    unchanged. Request bodies are read without rejecting unknown fields, so a server that
    predates `claim` ignores it and answers with an `open` task; the CLI and MCP see that
    status and claim the task next (`POST /v1/tasks/{id}/claim`), which another agent may
    win in between.
- `GET /v1/tasks/{id}` → `[TaskDetail]`. `history` is the task's newest 200 events, oldest
  first; when it was cut, it does not start with the `task_created` event.
- `PATCH /v1/tasks/{id}` `[UpdateTask]` → `[Task]`. `not_before` absent leaves it as it
  is, a time sets it, and `null` (or a time already passed) clears it.
- `owner` in `[UpdateTask]` hands the task over: the named agent owns it from then on, so
  its `task_done` succeeds and the previous owner's is refused. The owner is worked out
  after the request's status change (`open` clears it, `doing` sets the caller), and
  `owner` then replaces it; naming the owner that leaves changes nothing. Only the task's
  owner, the board's coordinator or the user may hand a task over (403 `forbidden`; another
  agent takes a stale owner's task first with `/take`). The status change is checked
  first, then: a task left without an owner (`open`, or unowned and not moving to `doing`)
  is refused with 400 `invalid` (claim it first, or resume it with `status: doing` and
  `owner`); a `done` or `dropped` task, or one moving to `dropped`, with 400
  `invalid_transition`; an unregistered target with 404 `not_found`; a target whose session
  ended or that is stale with 400 `invalid`. A task in `doing` counts against the new
  owner's `doing` limit (409 `claim_refused`, naming the owner's task in `doing`). Leases
  stay with the agents that took them. The `task_updated` event lists `owner` in `fields`
  with `owner` and `previous_owner`; the previous owner's snapshot alerts it to stop work
  on the task, and recent activity shows `handed <task> to <agent>`. `owner` is sent only
  when set; a server that predates it ignores it and answers with the owner unchanged, which
  the CLI and MCP report as an error.
- Within a minute of an open task's `not_before` passing, the server records one
  `task_updated` event by actor `server` with payload `{"fields": [], "not_before_passed":
  <time>}`; the task itself is unchanged (same `version`, `not_before` kept). The daemon
  wakes the coordinator if the task is then ready. Setting a new `not_before` announces it
  again once that passes. `not_before` is absent from a `Task` without one.
- `POST /v1/tasks/{id}/claim` `[TaskAction]` → `[Task]` with status `doing` and the caller
  as owner. Refused with 409 `claim_refused` when a live agent owns it or the caller is at
  its `doing` limit, and with 400 `invalid_transition` while a blocker is not done or its
  `not_before` has not passed. A claim
  is one conditional update in one transaction; exactly one of any concurrent claims wins.
- `POST /v1/tasks/{id}/release` `[TaskAction]` → `[Task]` back to `open`, no owner. Owner,
  board coordinator or user only (as status changes below).
- `POST /v1/tasks/{id}/take` `[TaskAction]` → `[Task]`. Only when `owner_stale`; records a
  `task_taken` event naming the previous owner. Taking an `open` task moves it to `doing`,
  so it is refused like a claim while a blocker is not done or `not_before` has not passed.
- `POST /v1/tasks/{id}/done` `[DoneTask]` → `[Task]`. Owner only; 400
  `evidence_required` without evidence, or with `check` evidence missing `exit_code`. The
  `task_done` event carries the evidence. An optional `summary` (1 to 600 characters,
  `SUMMARY_CHARS_MAX`) tells the user what changed and what they should know; it is stored
  on the task (`Task.summary`, absent without one) and listed in "Updates". It is not in the
  event; `task_show` shows it among the text written by other agents. Reopening the task
  clears it.
- `POST /v1/tasks/{id}/notes` `[AddNote]` → `[Note]`. Kind `feedback` is still accepted;
  agents report feedback through `POST /v1/feedback` instead.
- A task filed through `POST /v1/feedback` carries `feedback` (see "Feedback"); other tasks
  have none, and the field is absent.

Allowed status changes through `PATCH` (anything else is `invalid_transition`):

| From | To |
|---|---|
| `open` | `blocked`, `dropped` (claiming moves it to `doing`) |
| `doing` | `blocked`, `review`, `open` (same as release), `dropped` |
| `blocked` | `open`, `doing`, `dropped` |
| `review` | `doing`, `dropped` (`done` only through `/done`) |
| `done`, `dropped` | `open` (reopen) |

Who may change the status: anyone while the task has no owner or is `done` or `dropped`;
otherwise only its owner, the board's coordinator or the user (a `--user` token or the
browser), else 403 `forbidden`. Another agent takes a stale owner's task first
(`/take`, which is logged). Moving a task to `open`, from any status, clears its owner.
Moving it to `doing` makes the caller the owner, within the caller's `doing` limit; when
that changes the owner, the `task_updated` event lists `owner` in `fields` and carries
`owner` and `previous_owner`.

`blocked` needs `blocked_reason` or at least one blocker that is not done. Blockers must
exist and must not form a cycle; they may be on any board. A task is not `ready`, and
cannot be claimed, until every blocker is done, wherever it is. `GET /v1/tasks/{id}` lists
the tasks waiting on it in `blocks`, from every board, and a blocker's `task_done` event,
on its own board's feed, wakes the waiting task's board coordinator and the owner of a
task blocked on it (`DESIGN.md`, "Waking it"). Deleting a board removes its tasks' links
both ways: a task on another board that waited on one of them loses that blocker, with a
`task_updated` event on its own board, by the deleting actor, with payload `{"fields":
["blockers"], "blockers_removed": [ids], "board_deleted": slug}` and its `version` raised,
so an open task left with nothing to wait on is announced as ready (a done or dropped task
loses the link without an event). A blocked task's owner is not woken by this and sees the
change in its next snapshot. The temporary-board sweep never does this to a blocker that
is not done (see "Temporary boards").

### Leases and touches

- `POST /v1/leases` `[CreateLeases]` → `[LeaseResult]`. Advisory: overlapping leases of live
  peers are returned as `conflicts` and raise alerts; nothing is refused. A glob repeated in
  `globs` is leased once.
- `DELETE /v1/leases/{id}` → 204. Owner only.
- `POST /v1/touches` `[Touches]` → `[TouchResult]`: peers that leased or touched any of the
  paths within the last hour. 1 to 100 paths, each relative to the repo root, one line of at
  most 300 characters (`TOUCH_PATHS_MAX`, `PATH_CHARS_MAX`); globs in leases share the path
  limit. Optional: `scanned` (true when the paths were found by scanning the work tree after
  a shell call rather than named by an edit tool) and `modified` (path → modification time
  the client saw; only paths in the request, else 400). A scanned path whose time equals an
  unscanned touch of it by a peer on the same device is the peer's write: it is not recorded
  for the caller (nor in its `files_touched` event, skipped when no path is left), and a
  peer's unscanned touch removes a matching scanned one before overlaps are computed; alerts
  then skip the scan's earlier event for that path. A scanned path never replaces the
  caller's own unscanned touch of it. When a peer on the same device scanned the same path
  with the same time, the writer is unknown: both touches are kept, but neither counts as
  an overlap or alert between the two.

### Asks (decisions and actions for the user)

- `POST /v1/asks` `[CreateAsk]` → `[CreatedAsk]`: the `[Ask]`, plus an optional `warning`.
  Notifies the user (Telegram, if configured). When the board has other open asks younger
  than `duplicate_ask_window_secs` (taken from the server's clock), the ask is still
  created, and `warning` names each of them, newest first, with its id, task (or `no task`),
  asker (`you` for the caller) and age, and says to keep one and withdraw the other when
  one of them asks the same.
  `warning` is absent otherwise; clients that predate it read the answer as an `[Ask]`.
  `ask_user` (MCP) and `board ask` print it as a `warning:` line; `board ask --json` prints
  the answer unchanged.
- `GET /v1/asks?status=open&board=` → `[Ask]` list.
- `GET /v1/asks/{id}` → `[Ask]` in any status (the web view's `/asks/<id>` link).
- `POST /v1/asks/{id}/answer` `[AnswerAsk]` → `[Ask]`. User only; `answered_via` records
  `cli`, `web` or `telegram`. The `ask_answered` event's payload names the ask (`id`) and
  the asking `agent`, and the asker's next alerts include the answer.
- `POST /v1/asks/{id}/withdraw` `[TaskAction]` → `[Ask]` with status `withdrawn`. The
  asking agent only (403 otherwise); 400 `invalid_transition` unless the ask is open. It
  leaves the open queue, and its Telegram message is edited to say so and loses its
  buttons. The CLI (`board withdraw <id>`) and MCP (`ask_withdraw`) call it. After `ask_user`
  makes an ask on a task, it lists the task's other open asks and names them, with who
  asked, in a warning, so the agent can withdraw its own duplicates.

### Updates (user only)

What finished and what coordinators said, for the user, apart from the threads: a
coordinator's closing message can be followed at once by a wake-up that starts its next
turn, so the thread is not where the user reads it. One seen cursor is kept for the user
(every login in `users` and every `--user` token share it).

- `GET /v1/updates?since=&board=` → `[Updates]`: tasks done after `since` (`done`, as
  `[DoneUpdate]` with summary and evidence) and coordinators' turn endings after it
  (`said`), each newest first, at most 100 (`UPDATES_MAX`; `more` is true when one was
  cut). `since` defaults to the seen cursor; with neither, everything kept is listed.
  `board` limits both lists to one board. A task reopened since it was done is not listed,
  nor a feedback item closed by `resolve` (triage, not finished work). A server upgraded to
  updates starts the cursor at its newest done task, so earlier history counts as seen.
- `PUT /v1/updates/seen` `[MarkSeen]` `{at}` → `[SeenCursor]`. Moves the cursor forward to
  `at` (a later `at` than the server's clock counts as now; an earlier one than the cursor
  leaves it). Clients send the time of the newest update they showed, so updates that
  arrived after the page loaded stay unseen.

Neither records an event; agent tokens get 403 `forbidden`. A new turn ending arrives with
its coordinator's `agent_state_changed` event, and a new or reopened done task with
`task_done` or `task_updated`, so clients refresh updates on those.

### Feedback

Feedback is a report about any setup that cost an agent a step: the board, instructions,
skills, hooks, tools, storage or a project's setup. Each report lands on one task, a
feedback item, on the server's `feedback_board` (default `board-feedback`, created on first
use, autopilot off). Items get `auto` from that board's autopilot.

- `POST /v1/feedback` `[CreateFeedback]` `{about, text, activity?, source?, signature?}` →
  `[FeedbackResult]` `{task, duplicate, reports}`. Needs `X-Board-Agent`: the reporter.
  - `about` names the setup: 1 to 40 characters of `a-z 0-9 : _ -` (lowercased), such as
    `board`, `skills:rust` or `t3`; `other` when unsure.
  - `text`: the first line is the title, cut to 72 characters with an ellipsis; the whole
    text is the body (at most 4,000 characters).
  - `activity`: what the agent was doing, one of `explore`, `research`, `design`, `plan`,
    `build`, `review`, `operate`, `other`.
  - `source`: `agent` (default) or `capture` (a report written for the agent, such as by a
    hook). `signal` is the server's own (400 if sent).
  - `signature` (at most 120 characters) identifies the report for deduplication. Without
    one it is `<about>:<title>`, lowercased, with numbers, task ids and agent ids
    normalized as for signals.
- A report whose signature matches a **live** item adds to it instead of filing a new one
  (`duplicate: true`): `reports` grows by one, the reporter joins `reporters` (the latest
  20), and at most once an hour a `feedback` note says it was reported again, naming the
  routed task and its status when routed, with a `task_updated` event
  `{"fields": ["feedback"], "reports": n}`. A report leaves the task's `version` alone, so
  it never makes the owner's update conflict. The newest item with the signature is live
  while it is unresolved and not done or dropped, declined, a duplicate of a live item (the
  link is followed), or routed to a task that is not done or dropped.
- Otherwise a new item is filed (`task_created` with a `feedback` payload). When the newest
  item with the signature is no longer live (its routed task is done or dropped, it was
  forwarded or fixed, or it was closed without a resolution), the new item's `recurs` names
  it: the fix did not hold, or it came back.
- `POST /v1/feedback/{id}/resolve` `[ResolveFeedback]` `{kind, task?, reason?}` → `[Task]`.
  The item's owner (claim it first) or the user only; 403 `forbidden` otherwise, with the
  next step. `kind` is one of:
  - `routed`: `task` (any board) does the work. Reports go to this item until that task is
    done or dropped; then a report opens a new item that `recurs` this one.
  - `duplicate`: `task` is the original item, which takes later reports.
  - `forwarded`: handed elsewhere (another tracker, the user); `reason` says where.
  - `declined`: not acted on; `reason` says why. It stays live, so repeats keep counting.
  - `fixed`: the resolver fixed the cause itself; `reason` says what changed (such as the
    file and line, or the setting). It is not live: a later report opens a new item that
    `recurs` this one.

  `task` is required for routed and duplicate and refused for the others; `reason` (one
  line, at most 280 characters) is required for forwarded, declined and fixed. Any other
  `kind` is 400 `invalid`. The item moves to
  `done` with artifact evidence naming the resolution (`feedback routed to factory-12`), and
  its `task_done` event carries `resolution`. 400 `invalid_transition` when the item is
  already done or dropped (reopen it first; reopening clears the resolution); 400 `invalid`
  when the task is not a feedback item or `task` does not exist. A plain task on the
  feedback board (added with `task_add`, or filed before feedback items existed) is resolved
  as an item about `other`, reported once by its creator, with the signature of its title.

`Task.feedback` `[Feedback]`: `about`, `activity` (absent when not given), `source`,
`signature`, `reports`, `reporters`, `last_reported_at`, `resolution` (absent until
resolved: `{kind, task?, reason?, at, by}`, where `by` is an agent id or `user`) and
`recurs` (absent unless it recurs an earlier item).

### Snapshots and events

- `GET /v1/snapshot?agent=&kind=full|delta|alerts&since=&format=lines|json|yaml` →
  `[Snapshot]`. Rendered on the server (format in `DESIGN.md`, "Text format"), so every
  client injects the same text. `delta` and `alerts` with nothing relevant return empty
  `text`. The server also advances the agent's cursor to the returned `seq`. One request
  checks at most 1,000 events that can concern the agent for alerts; past that, `seq` is the
  last one checked and the first alert says how many wait for the next request. Alerts read
  only the last 10,000 events; an alert names the older ones skipped. `json` is
  compact, so all three formats share the same size caps.
- `GET /v1/events?since=&board=&limit=` → `[EventsPage]`. With
  `Accept: text/event-stream`, streams events as SSE (`id: <seq>`, `data: <Event JSON>`),
  resuming from `Last-Event-ID`, else from `since`. Events older than 365 days are pruned
  (the newest event always stays), so the events just after an old `since` may be gone.

### Administration (user only)

- `POST /v1/admin/devices` `[CreateDevice]` → `[DeviceToken]` (token shown once).
- `GET /v1/admin/devices` → `[DeviceInfo]` list, never with tokens.
  `DELETE /v1/admin/devices/{name}` revokes; a revoked name may be created again.
- On the server host, `board admin device <name> [--user] --db <file>` creates a device
  without HTTP (the first user token comes from here), `board admin telegram-pair
  [--replace] --db <file>` prints a one-time Telegram pairing code (see `DEPLOY.md`), and
  `board admin backup --db <file> --out <new file>` writes a consistent copy.

### CLI output for scripts

`--json` prints the server's answer as this `board` read it, pretty-printed: fields it does
not know are left out, fields an older server lacks get their defaults, and values of open
enums it does not know read `unknown` (see "Compatibility"). Scripts, such as a weekly
review, may rely on these shapes, which follow `/v1`'s compatibility rules:

- `board stats [--days N] --json` → `[Stats]` from `GET /v1/stats?days=N` (default 7):
  `days`, `tasks_created`, `tasks_done`, `claims_refused`, `tasks_taken_stale`,
  `lease_conflicts`, `asks_created`, `asks_answered`, `ask_answer_minutes_median`,
  `doing_minutes_median` (minutes, `null` when nothing was measured),
  `done_without_check_evidence` and `feedback` `[FeedbackStats]`: `reported` (reports
  received, repeats included), `filed` (items created) and `resolved` (`routed`,
  `duplicate`, `forwarded`, `declined`, `fixed`; `fixed` reads 0 from servers that predate
  it), each within the period. Without `--json` it prints
  a four-line summary.
- `board ls --board <slug> --json` → `[Task]` list from `GET /v1/tasks?board=<slug>`, for
  example `board ls --board board-feedback --json` for the feedback items. It returns at
  most 200 tasks; pass `--limit <n>` (up to 1,000) for more.
- `board feedback [--about A] [--activity X] [--source agent|capture] [--signature S] <text>
  --json` → `[FeedbackResult]` from `POST /v1/feedback`; `--about` defaults to `other`.
- `board resolve <id> (--routed <task> | --duplicate <task> | --forwarded <reason> |
  --declined <reason> | --fixed <what changed>) --json` → `[Task]` from `POST /v1/feedback/{id}/resolve`.

All exit 1 with the error on stderr when the server refuses or cannot be reached.

### Web view

Every `GET` outside `/v1` and `/healthz` serves the embedded web view: files under
`/assets/` with immutable caching, any other path without a file extension as
`index.html` (`no-cache`). Every web view response, `/index.html` included, carries a
Content-Security-Policy that allows only this origin and forbids framing, plus
`X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`.

## Defaults

| Setting | Default |
|---|---|
| Owner counts as stale after | 24 hours unseen |
| File lease and coordinator lease TTL | 45 minutes, renewed by heartbeats |
| Touch overlap window | 1 hour |
| Tasks in `doing` per agent | 1 (claiming a second is refused unless the first is blocked or in review) |
| Done tasks hidden from snapshots after | 30 days |
| Idle temporary board closed after (`temporary_close_after_secs`) | 1 day |
| Idle closed temporary board deleted after (`temporary_delete_after_secs`) | 7 days |
| Snapshot size caps | full 2,000, delta 600, alerts 400 characters |
| Text limits | title 72, note 500, handoff field 300, question 1,000, body 4,000 characters; outside settings, ask option 60 and evidence value 500 |
| Feedback board (`feedback_board`) | `board-feedback`, created on first use |
| Signal: errors, agents, window (`signal_min_reports`, `signal_min_agents`, `signal_window_days`) | 3 errors from 2 agents within 7 days |
| Other open asks on the board a new ask warns of (`duplicate_ask_window_secs`) | younger than 30 minutes |

All defaults are server settings, tuned through the feedback loop (`DESIGN.md`).
