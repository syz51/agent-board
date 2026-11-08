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
`N`, task ids `ID`, agent ids `AGENT`, quoted text `"…"`), with the source `server`. It also
records the refusals a client made before sending (`POST /v1/errors`), with the source
`client`, under the signature its own answer would get, so both count towards signals and
`GET /v1/errors`. Once the same code, route and template reach
`signal_min_reports` errors from at least `signal_min_agents` agents within
`signal_window_days`, the server files feedback itself (see "Feedback") with source
`signal`, about `board` and signature `signal:<code>:<hash>`, giving the count, the agents,
the window, the route and the template. Later errors add reports to that item while it is
live. Once it is no longer live (its fix is done), only errors after its last report count
towards a new item, which `recurs` it. Records are kept `error_retention_days`, and never less than the window,
so `GET /v1/errors` can count errors under the threshold, such as since a fix deployed; the
server's minute sweep deletes older ones.

## Endpoints

### Health

- `GET /healthz` → `{"ok": true, "seq": <latest event>, "version": "<x.y.z>", "commit":
  "<12 hex digits or unknown>", "schema_version": <migrations run>}`; no authentication.
  `version`, `commit` and `schema_version` are absent from servers that predate them.
- Every answer, errors included, carries `X-Board-Version` and `X-Board-Commit` with the
  same values. The CLI, the MCP server and the daemon compare them with their own build:
  they differ by version, or by commit when both know theirs. Each server build that
  differs is reported once per process: the CLI on stderr, the MCP server in the next tool
  result, the daemon in its log. Hooks stay silent, and header values that are not plain
  tokens are ignored. `board --version` prints `board <version> (<commit>)`; the commit is
  `BOARD_COMMIT` when set at build time, else the checkout's `HEAD`.
- Every answer also carries `X-Board-Built`, the build's order stamp: its commit's committer
  time in seconds since the Unix epoch (`BOARD_BUILT` when set at build time, else from
  git), or `unknown`. A newer build has a higher stamp, so the warning names the older side.
  A server that sends no stamp predates it and counts as older. When the server is older,
  the warning says to upgrade it (docs/DEPLOY.md). When the client is older, it says that
  calls still work and what picks up the new build. An MCP server or daemon reads the commit
  of the board installed at its own path: the same as the server's means a restart of the
  session (or `board watch`) does; its own means the device upgrade does. A stamp of
  `unknown` on either side, or equal stamps, give the warning without an order.

### Agents

- `POST /v1/agents` `[RegisterAgent]` → `[Agent]`. Idempotent; refreshes `last_seen` and
  replaces repo, branch and cwd (where the agent runs now; one left out is cleared). A
  `title` or `forked_from` left out keeps the stored one, and `native_ids` are merged into
  the stored ones. A new id with `forked_from` starts with no tasks. A device token
  registers agents only for its own `device` (403 otherwise); the user may name any device. An agent named
  as a board's coordinator keeps the `coordinator` role whatever role it registers with,
  and its stored `title` unless it registers as `coordinator` (the hooks register a T3
  thread's own title as a worker; the daemon names its coordinator after the board). An
  agent stored with the `coordinator` role keeps its title against a worker registration
  too, since the daemon registers its coordinator before it claims the lease. The hooks
  send a T3 thread's link as native id `t3_link`: it must start with `https://`, have more
  after it and hold no white space (400 `invalid` otherwise). Native id values are at most
  300 characters and a `title` at most 200 (`NATIVE_ID_CHARS_MAX`,
  `AGENT_TITLE_CHARS_MAX`).
  Clients register on session start (hooks) and before acting when the agent is unknown.
  Registering marks the agent `running`; one that was `idle`, `paused` or `ended` gets one
  `agent_state_changed` event (`from` its old state, `to` `running`).
- Every call that acts as an agent marks it seen. One marked `ended` that makes such a call
  itself (a task, note, lease, touch, ask, handoff, board join, feedback call, or snapshot
  that advances its cursor) is `running` again, announced by one `agent_state_changed`
  event (`from` `ended`, `to` `running`). Calls made for it leave its state alone: a state
  update and task changes through the user token mark it seen; a device claiming or
  renewing the coordinator lease for it does not, so `last_seen` says when the agent itself
  last acted.
- Wherever a request names an agent to look up (`owner` in `[UpdateTask]` and in
  `GET /v1/tasks`), its short handle works too.
- Clients act only as themselves: in a T3 thread, outside a nested session, `--agent`
  (CLI) and `agent=` (MCP) may name only the thread's own agent, and any other id is
  refused before anything is sent, with an error that says to drop the flag. Elsewhere,
  where the environment proves no identity, any agent may be named. The server does not
  check this.
- `GET /v1/agents?board=&state=` → `[Agent]` list. With `board`, agents not seen for more
  than 7 days are left out whatever their state, since a session that crashed never sends
  `ended`. Kept are agents that wait for a usage-limit reset still to come (`paused_until`
  ahead, also when ended), agents `paused` with no reset (they wait for the user), and the
  board's coordinator while its lease is live (the daemon renews the lease without marking
  the agent seen). `state=ended` lists every ended agent. The agents left out stay members
  of the board, and a call that marks one seen (a registration, a state update or another
  write as that agent; reads do not) lists it again. (Servers before this change left out
  only agents that ended more than 7 days ago.)
- `GET /v1/agents/{id}` → `[Agent]`.
- `POST /v1/agents/{id}/state` `[AgentStateUpdate]` → `[Agent]`. Heartbeat and lifecycle.
  `ended` releases the agent's file leases; owned tasks stay owned. Its coordinator lease
  runs on until it expires: T3 ends a session left idle for 30 minutes while the thread
  lives on, and the daemon of the lease's device renews the lease while the thread is live,
  or releases it when the thread is gone or T3 cannot say. (Servers before this change
  released the coordinator lease too.)
  `paused_until` is accepted only with `paused`. An agent paused until a time still ahead
  keeps that `paused_until` when it is marked `ended` (T3 closes an idle session, but the
  usage limit holds until its reset), and the `agent_state_changed` event carries it; any
  other state, a registration or a call that revives the agent clears it.
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
  A `Board`'s `counts` hold the tasks per status, `ready` (see Tasks) and `ready_auto`: the
  ready tasks with `auto` on, which the coordinator may start. The rest of `ready` waits on
  the user. `board boards` and the snapshot counts line show both as `ready N (auto M)`;
  `json` and `yaml` snapshots name it `auto` under `counts`. A server that predates
  `ready_auto` leaves it out, and `board boards` then shows `ready N` alone.
- `GET /v1/boards/{slug}` → `[Board]`. `PATCH /v1/boards/{slug}` `[UpdateBoard]` → `[Board]`.
  `parallel_tasks` (1 to 4; 400 `invalid` otherwise) is how many tasks the board's
  coordinator and its child threads may have in progress at once. The coordinator keeps to
  it; the server does not refuse claims over it. Raising it is user only (403
  `forbidden` otherwise), since more tasks at once spend the shared usage limit; anyone may
  restate or lower it. A `Board` from a server that predates it reads as 1, and a client
  leaves the field out of `UpdateBoard` unless it sets it.
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
- `GET /v1/stats?days=7` → `[Stats]`. `days` is 1 to 365, default 7. Its fields are listed
  under `board stats` in "CLI".
- `GET /v1/errors?since=&code=&route=&q=&limit=` → `[ErrorLog]` `{since, retention_days,
  signatures}`: the API errors recorded for signals (see "Signals") at or after `since`,
  one row per signature, most errors first. A row `[ErrorSignature]` has `signature`
  (`signal:<code>:<hash>`), `code`, `route` (the template), `template` (the normalized
  message), `count`, `agents` (distinct), `first_at`, `last_at` and `item`: the newest
  feedback item with that signature, or `null` when none was filed, as for errors under
  the signal threshold. `since` is an RFC 3339 time, default `signal_window_days` back from
  now; the server keeps records `error_retention_days`, so an earlier `since` reads from the
  oldest kept, and the answer's `since` says so. `code` is an error code (400 `invalid`
  otherwise) and `route` a route template, matched exactly; `q` (at most 200 characters)
  matches the template's text, ignoring ASCII case. Blank `route` and `q` are left out.
  `limit` is 1 to 1,000, default 100. Each row counts both sources.
- `POST /v1/errors` `[ClientError]` `{code, route, message}` → 204: records a request the
  calling agent's client refused before sending, such as text over a limit, as if the
  server had answered it (see "Signals"): source `client`, the same signature, and a signal
  item once the threshold is reached. `code` is `invalid`, the only code recorded from
  clients; `route` is the method and route template the request would have gone to, one of
  the `/v1` routes in this document but this one (`POST /v1/tasks/{id}/notes`); `message`
  is the server's message for text over a limit, exactly as the server writes it
  (`<field> is <n> characters; the limit is <max>`, `n` above `max`), for a field clients
  check: `title`, `body`, `acceptance`, `blocked_reason`, `evidence value`, `text`, `now`,
  `next`, `blockers`, `question`, `option`, `answer`, `q`, `signature`, `reason` or `goal`
  (`board_proto::CLIENT_CHECKED_FIELDS`), so a record carries no text of the client's
  choosing. It needs a device token and `X-Board-Agent`, and the record is that agent's:
  the body names no agent. Anything else is 400 `invalid`. Servers before it answer 405,
  or 404 `not_found` before `GET /v1/errors`; some of those record that 404 as their own
  error.

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

- `GET /v1/tasks?board=&status=&owner=&ready=&proposal=&mine=&limit=` → `[Task]` list,
  sorted by priority, then id number. `mine=true` uses the `X-Board-Agent` header.
  `proposal=true` lists only pending proposals, `false` leaves them out (see "Pending
  proposals" below). `limit` defaults to 200, at most 1,000.
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
    A body starting `Proposal:` with `claim: true` is 400 `invalid`: the agent that files a
    proposal cannot start it.
  - A body starting `Proposal:` (after leading whitespace) creates a pending proposal: the
    server stores `auto: false` whatever the request says.
  - Compatibility: `claim` is sent only when true, so older clients' requests are
    unchanged. Request bodies are read without rejecting unknown fields, so a server that
    predates `claim` ignores it and answers with an `open` task; the CLI and MCP see that
    status and claim the task next (`POST /v1/tasks/{id}/claim`), which another agent may
    win in between.
- `GET /v1/tasks/{id}` → `[TaskDetail]`. `history` is the task's newest 200 events, oldest
  first; when it was cut, it does not start with the `task_created` event. `handles` maps
  each registered agent the detail names (owner, creator, note and evidence authors,
  history actors, feedback resolver) to its short handle when that differs from its id;
  it is absent when none does, and `task_show` names agents by these handles.
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
- Within a minute of a task in `review` passing `review_quiet_secs` with no note from its
  owner, the server records one `task_updated` event by actor `server` with payload
  `{"fields": [], "quiet_review": <time>, "in_review_since": <time>}`: `quiet_review` is the
  owner's last note or the move to `review`, whichever is later, and `in_review_since` the
  move. The task is unchanged. While it stays quiet the event repeats every
  `review_quiet_secs`; a note from the owner or a new move to `review` restarts the clock.
  The snapshot shows it as an alert to the owner, or to the board's coordinator when the
  owner has ended or is not live.
- Within a minute of a task in `doing`, on an active board, passing `doing_idle_secs` with
  its owner away, the server records one `task_updated` event by actor `server` with
  payload `{"fields": [], "idle_doing": <time>, "away": <agent id>}`: `idle_doing` is when
  the spell away began, `away` the owner then (unlike `owner` in a handover, it names no
  new owner). Away means the owner has not run since its first state change after its
  last change to `running`, or is marked `running` but was last seen `doing_idle_secs`
  ago. An owner waiting for a usage-limit reset still to come (`paused`, or `ended` with
  that `paused_until` kept) is not away. No event is recorded while an agent the owner
  delegated (a T3 thread whose `t3_parent` chain leads to the owner) is running and was
  seen within `doing_idle_secs`, or waits for a reset still to come (`paused` or `ended`).
  The task is unchanged. The event is recorded at most twice per spell. The spell's
  events are those for the same task and owner after the owner's last change to `running`
  and after the task last came to it (claimed, taken, moved to `doing`, handed over); a
  session ending later is the same spell. The first is recorded once the owner has been
  away `doing_idle_secs`. A second, with the same payload, is recorded once it has been
  away twice `doing_idle_secs`, unless the first was recorded that late; no third. A running owner's spell starts when it was last seen, so
  one seen again starts a new one. The snapshot shows it as an alert while the task is
  still in `doing` and still that owner's: to the owner, and to the board's coordinator
  when the owner is no T3 thread or has been away at least twice `doing_idle_secs`.
  `board watch` on the owner's device wakes a T3 owner. It wakes the board's coordinator
  instead when the owner is no T3 thread, or when the owner's thread is gone and the
  coordinator runs on the owner's device. Recent activity shows it as `found <agent> away
  from <task>`.
- `POST /v1/tasks/{id}/claim` `[TaskAction]` → `[Task]` with status `doing` and the caller
  as owner. Refused with 409 `claim_refused` when a live agent owns it or the caller is at
  its `doing` limit, and with 400 `invalid_transition` while a blocker is not done or its
  `not_before` has not passed. A pending proposal is refused the same way, with 400
  `invalid_transition`, until a verdict sets it `auto`. A claim
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

Pending proposals: a task is one while it is `open`, not `auto`, not a feedback item (whose
body is a reporter's text, triaged instead), and its body starts `Proposal:` after leading
whitespace (`board_proto::is_pending_proposal`, one definition for the server, the daemon,
the CLI and the web view). The rule reads the task as it is now: an edit that sets `auto`
or removes the prefix ends it. `Task.proposal` says so; it is computed when read and absent
from servers that predate it (clients apply the rule to the task's fields instead). A
pending proposal is never `ready` and cannot be claimed. No other path takes an
unaccepted proposal (not `auto`, with the prefix, whatever its status) into `doing`:
`PATCH` to `doing`, from `blocked` say, is refused with 400 `invalid_transition` unless
the same request sets `auto`. The snapshot lists pending proposals under `proposals`,
apart from ready work, with who filed each (an agent does not review its own); one held by
a `not_before` still ahead is listed under `waiting` instead. A proposal becomes work when
another agent records a `verdict:` note and an accept sets `auto` (`DESIGN.md`,
"Proposals and triage").

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
  `globs` is leased once. Each lease in the answer names its repo. With a `task` whose
  board lists repos and not `repo`, the lease is still taken in `repo`, and the answer's
  `warning` (absent otherwise) names the board's repos: peers working the task there get no
  warning from this lease. With a `task` that is `done` or `dropped`, the leases are taken
  without a task and the `warning` says so. Leasing a glob the caller already holds renews
  the same lease; when the live lease named another task, or none, the lease then names no
  task and the `warning` lists those globs, so closing one task never ends a lease another
  still needs. Several warnings are joined with `; `. `lease_paths` (MCP) and `board lease`
  print it as a `warning:` line above the leases.
- `DELETE /v1/leases/{id}` → 204. Owner only.
- Leases end with their task. When a task goes to `done` (`/done`, or a feedback item's
  `/resolve`), `dropped` (`PATCH`, by whoever may move it) or `open` (`/release`, or
  `PATCH`), every lease whose `task` names it is deleted, whoever holds it, in the same
  transaction: work on it has stopped, and an open task's next owner should not be warned
  about its own files. Each records `lease_released` on the task's board, with the task, by
  the agent that moved it, with payload `{id, agent, repo, glob}` (`agent` is the holder, as
  in `lease_expired`; an explicit `DELETE` records `{id, repo, glob}` by the holder, with no
  board). A task moved to `blocked` or `review`, or handed to another owner, keeps its
  leases. (Servers before this change kept such leases until their holder ended or they
  expired.)
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
  leaves it). A client should send the time of the newest update it showed, so updates
  that arrived after it loaded stay unseen. The web view no longer shows updates, so
  nothing it does moves the cursor.

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
  task doing the work and its status (the routed task, or a plain task the item is a
  duplicate of), with a `task_updated` event `{"fields": ["feedback"], "reports": n}`.
  The same hourly report also adds a plain `note` to that task while it is not done or
  dropped, so its owner sees the new evidence in its alerts. The note gives the item, the
  report count, the number of distinct reporters, their ids (newest first, cut at the note
  limit) and the report's first line, quoted as data written by another agent; for a
  signal, the error's summary. Notes record the reporter as author; the `note_added`
  event's actor is `server` for a signal. A report leaves the task's `version` alone, so
  it never makes the owner's update conflict, except the one that reopens a declined item
  (below), which changes its status. The newest item with the signature is live
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
  - `duplicate`: `task` is the original item, which takes later reports. The original must
    still take reports: live as above (a plain task: not done or dropped). Otherwise 400
    `invalid`, since a duplicate of an item that is no longer live is not live either and
    the next report would open yet another item. The hint says where to route this item
    instead (kind `routed`): the open revisit of the original's routed task (a child task
    whose title starts `Revisit:`) by id; else, when that task is dropped, its verdict
    (decline citing it, or file a new proposal with evidence it did not weigh); else a
    `Proposal:` task on that task's board, naming the task and its owner; else, when the
    original has no routed task (fixed, forwarded, closed unresolved, or its task is
    gone), a proposal for the setup's owner. Also 400 `invalid` when the resolved item
    would take no reports because the duplicate links lead back to it.
  - `forwarded`: handed elsewhere (another tracker, the user); `reason` says where.
  - `declined`: not acted on; `reason` says why. It stays live, so repeats keep counting.
    When agents keep reporting it, the server sends it back to triage. The check runs on
    each report to a declined item. It needs three things: the item was declined at least
    `declined_reopen_after_days` ago; it has gained at least `declined_reopen_reports`
    reports since the decline, from any agent; and at least `declined_reopen_agents` of
    the agents behind them are new. A new agent had not reported it before the decline:
    not on this item, not on an item resolved as its duplicate, and not as its creator.
    The report that meets all three moves it to `open`, unowned, with the resolution
    cleared and `version` raised. Reports that met the bar during the wait take effect
    with the next report after it. A `finding` note, credited to that
    reporter, starts `reopened:` and gives the reports and agents since the decline, how
    many agents are new, the decline time and its reason. It comes with `note_added` and a
    `task_updated` event `{"fields": ["status", "feedback"], "from": "done", "to": "open",
    "reopened": {"reports", "agents", "new_agents"}}`; when the item still had an owner,
    `fields` also lists `owner`, and the event carries `owner` (null) and `previous_owner`. Every report counts,
    not just the 20 in `reporters`. Declining it again starts a new count.
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
  `text`. Alerts come from events since the cursor. The server also advances the agent's
  cursor to the returned `seq`. One request checks at most 1,000 events that can concern the
  agent for alerts; past that, `seq` is the last one checked and the first alert says how
  many wait for the next request. Alerts read only the last 10,000 events; an alert names
  the older ones skipped. `json` is
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
  `done_without_check_evidence`, `done_under_a_minute` (tasks done after under a minute in
  `doing`, not counting feedback items or a done right after a takeover; `null` or absent
  from servers that predate it) and `feedback`
  `[FeedbackStats]`: `reported` (reports
  received, repeats included), `filed` (items created) and `resolved` (`routed`,
  `duplicate`, `forwarded`, `declined`, `fixed`; `fixed` reads 0 from servers that predate
  it), each within the period; `since`, the oldest time the numbers cover (`days` back
  from now, or the server's oldest kept event when that is later; absent from servers that
  predate it); and `boards` (empty from servers that predate it), one `[BoardStats]` per
  board with events in the period, by slug: `board`; `claims`, claims of auto tasks that
  waited ready (tasks created claimed are left out), with `wait_minutes_median` and
  `wait_minutes_p90` from ready to claimed. A task is ready at the latest of: its last
  creation, release, move to `open`, or change of `auto`, blockers or `not_before`; the
  `not_before` it has then (the one it was created with until a change); and its last
  blocker done. A cleared or earlier `not_before` makes it ready sooner. `tasks_done` and
  `done_per_day`; `doing_minutes_median`, time in `doing` since the task was last done,
  with `review` excluded; `lease_conflicts`, lease conflicts and file-touch overlaps in the
  board's repos, whichever task a lease names, if any; `reopened_after_review`, moves from
  `review` to `open`, `doing` or `blocked`; `pr_links` in the evidence of the tasks done;
  `limit_pauses` of member agents after they joined, and `limit_pauses_per_day`;
  `paused_hours`, until each pause's reset, an earlier move to a working state, the
  session's end when the pause named no reset, or now (a repeated pause before the reset
  extends it to a later reset named; one after the reset is a new pause); and `peak_running_at_pause`, the most member agents running when one
  paused (`null` without pauses). A member counts as running when its latest registration
  or state change before the pause left it running and it acted within the owner
  staleness threshold before it. Rates are per day since `since`, and medians and
  percentiles are `null` when nothing was measured. Without `--json` it prints a summary,
  says when the history starts more than an hour into the period asked, and adds one line
  per board.
- `board errors [--since T] [--code C] [--route R] [--grep TEXT] [--limit N] --json` →
  `[ErrorLog]` from `GET /v1/errors`. `--since` takes an RFC 3339 time, a date
  (`YYYY-MM-DD`, midnight UTC) or a duration back from now (`30m`, `12h`, `14d`); `--grep`
  is sent as `q`. Without `--json` it prints a header naming the window, then one line per
  signature: count, code, distinct agents, route, item, template and last time.
- `board ls --board <slug> --json` → `[Task]` list from `GET /v1/tasks?board=<slug>`, for
  example `board ls --board board-feedback --json` for the feedback items. It returns at
  most 200 tasks; pass `--limit <n>` (up to 1,000) for more.
- `board feedback [--about A] [--activity X] [--source agent|capture] [--signature S] <text>
  --json` → `[FeedbackResult]` from `POST /v1/feedback`; `--about` defaults to `other`.
- `board resolve <id> (--routed <task> | --duplicate <task> | --forwarded <reason> |
  --declined <reason> | --fixed <what changed>) --json` → `[Task]` from `POST /v1/feedback/{id}/resolve`.

All exit 1 with the error on stderr when the server refuses or cannot be reached.

Text over a limit (see "Defaults") is refused before any request, with the server's message
followed by `(not sent)` and its `next` hint, and exit 1. The CLI then reports the refusal
to `POST /v1/errors`, waiting at most 2 seconds, so signals count it; it does not report an
over-long feedback `about`, which the server refuses with another message, or a request it
would not have sent, such as `board handoff --blockers` alone, which only shows the
handoff. The report never
changes the output or the exit code: a server that is unreachable, slow, older or refuses
it is ignored. MCP tools do not check lengths before sending, so the server answers and
records those itself.

### Web view

Every `GET` outside `/v1` and `/healthz` serves the embedded web view: files under
`/assets/` with immutable caching, any other path without a file extension as
`index.html` (`no-store`). Every web view response, `/index.html` included, carries a
Content-Security-Policy that allows only this origin and forbids framing, plus
`X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`. Pages (`index.html`
and the client routes) also allow `<style>` elements that carry a nonce: each page response
has a fresh random one in its `style-src` and on the page's `csp-nonce` meta. Scripts get
no nonce.

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
| API error records kept (`error_retention_days`) | 35 days, and never less than the signal window |
| Declined feedback item reopened after, with reports and new agents since the decline (`declined_reopen_after_days`, `declined_reopen_reports`, `declined_reopen_agents`) | 1 day, 5 reports, 3 agents |
| Other open asks on the board a new ask warns of (`duplicate_ask_window_secs`) | younger than 30 minutes |
| Task in `review` with no note from its owner announced after (`review_quiet_secs`) | 1 day, then again each quiet day |
| Task in `doing` whose owner is away announced after (`doing_idle_secs`) | 30 minutes, once per spell away |

All defaults are server settings, tuned through the feedback loop (`DESIGN.md`).
