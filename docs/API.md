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
- New values of the open enums `ResolutionKind` (`Feedback.resolution.kind`),
  `EventType` (`Event.type`), `DeviceRole`, `DeviceStatus`, `ResourceWeight`,
  `TaintEffect`, `RunKind`, `RunOrigin`, `RunState`, `RunEventKind`, `HousekeepingAction`
  and `DispatchHoldReason`: clients read a
  value they do not know as `unknown`
  (`#[serde(other)]` in `board-proto`; `oneOfOrUnknown` or a plain string in the web view),
  so the item or event still reads and renders. The server never stores `unknown` and
  refuses it in a request (400 `invalid`); a server rolled back past a kind it stored reads
  that kind as `unknown` too. `ResolutionKind` became open in the same release that added
  `fixed`, so clients from before that release fail to read an item resolved `fixed`:
  restart `board mcp` processes and daemons after upgrading.

Every other enum is closed: a new value would fail every read that carries it in a client
that predates it. Add one only after making the enum open as above and waiting until running
clients have been restarted on a version that reads it.

Agent id sources are closed too. The `dispatch` source (see "Daemon dispatch") is new:
clients before it fail to read an answer that carries a `dispatch:` id, such as a board
whose coordinator it is or a task it owns. Upgrade and restart every client (`board mcp`,
daemons) before a board's dispatch agent takes its coordinator lease. The `opencode`
source (`opencode:<session id>`) and the `Runtime` value `opencode` are new in the same
way: upgrade and restart every client before an opencode session registers.

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
(`:` as `%3A`, `%` as `%25`). A device token may send `X-Board-Agent: dispatch:<device>`
only for its own device (403 `forbidden` otherwise); a `--user` token may name any.

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
towards a new item, which `recurs` it; when it was resolved as a duplicate, errors after the
last report of the items its duplicate links lead to. Records are kept `error_retention_days`, and never less than the window,
so `GET /v1/errors` can count errors under the threshold, such as since a fix deployed; the
server's minute sweep deletes older ones.

## Endpoints

### Health

- `GET /healthz` → `{"ok": true, "seq": <latest event>, "version": "<x.y.z>", "commit":
  "<12 hex digits or unknown>", "schema_version": <migrations run>, "features": [...]}`;
  no authentication. `version`, `commit`, `schema_version` and `features` are absent from
  servers that predate them.
- Every answer, errors included, also carries `X-Board-Features` with the server's
  comma-separated protocol and capability feature tokens (for example
  `v1-json,sse-v1,device-caps-v1,usage-buckets-v1,runs-v1,dispatch-claim-v1,scheduler-telemetry-v1,run-events-v1`)
  for additive feature negotiation.
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
  A dispatch agent (`dispatch:<device>`, see "Daemon dispatch") registers with its own
  device as `device` and the runtime of the threads it starts. Another device's token is
  refused with 403 `forbidden` (see "Authentication"); a `--user` token that names another
  `device` gets 400 `invalid`.
  Its handle is its id.
  Clients register on session start (hooks) and before acting when the agent is unknown.
  The client registers before acting only the agent its environment names: an id named by
  hand, by `--agent` (CLI, also `board mcp --agent`) or `agent=` (MCP), that the server does
  not know is refused before anything is written, with an error that says how to act as
  yourself; a new agent registers through its session's hooks or the daemon. A T3 thread's
  own id counts as not named, and so does an `agent=` that repeats the agent `board mcp`
  acts as. A script that acts under its own fixed id passes `--register`
  with `--agent` to register it; the hooks register an explicit `--agent` as before.
  (Clients before this change registered any unknown id named this way.)
  Registering marks the agent `running`; one that was `idle`, `paused` or `ended` gets one
  `agent_state_changed` event (`from` its old state, `to` `running`).
  For every non-`dispatch:` session whose `device` exists in the fleet registry,
  `POST /v1/agents` also updates unified runs in the same transaction: if the agent is
  already linked to an active `origin: managed` `Run`, its non-null session metadata
  (`worktree_branch`, `cwd`, `title`, `attach_hint` — overriding an existing managed
  `attach_hint` only with an explicit `t3_link`, using `ssh://<device><cwd>` only to fill an
  unset `attach_hint` — and unset `task_id`/`board`) is synced
  onto that managed run without creating a duplicate; otherwise the server upserts an active
  `origin: interactive`, `kind: interactive` `Run` (`cost_units: 2`, `state: running`,
  `runner: "t3"` when the agent is a `t3:` thread or carries `t3_link`/`t3_thread`, else
  the agent's `runtime`; `attach_hint`: `t3_link` else `ssh://<device><cwd>` when within
  500 characters; `board` from its `doing` task, else the single active board for its `repo`
  or the single active board it joined). Its id is deterministic per active session
  (`run-interactive-<12-hex of SHA-256(agent_id)>`, updated in place across re-registrations;
  once ended, a later session starts a fresh terminal-preserving run suffixed `-2`, `-3`, …).
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
  (CLI, also `board mcp --agent`) and `agent=` (MCP) may name only the thread's own agent,
  and any other id is refused before anything is sent, with an error that says to drop the
  flag. Elsewhere, where the environment proves no identity, any agent the server knows
  may be named. The server does not check this.
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
  In the same transaction, any active `origin: interactive` `Run` for the agent tracks the
  state (`running` → `running`, upserting one if none is active and no managed run owns the
  agent; `idle`/`paused` → `waiting_external`; `ended` → `succeeded` with `ended_at = now`,
  releasing any held `conflict_locks` and expiring pending `sticky_wakes`).
  Managed runs are never transitioned by agent state hooks.
- Within a minute of an agent marked `running` going unseen for `owner_left_secs` (an
  hour by default), the server marks it `ended`: its session stopped without an end hook.
  It skips an agent with `paused_until` ahead, one with a thread it delegated at work (as
  the idle `doing` notice judges), the coordinator of a board whose lease is live, a live
  judge, and the daemon's own `dispatch:` agents. Like an `ended` update, this releases the
  agent's file leases, closes any active `origin: interactive` `Run` to `succeeded` (with
  `ended_at = now`), and keeps its tasks and coordinator lease; it keeps a judge's rights
  too. The `agent_state_changed` event, by the agent, carries `from` `running`, `to`
  `ended`, `paused_until` `null` and `unseen_since` (when it was last seen); `last_seen` is
  unchanged. An agent that acts again is `running`, as above. A session that waits over
  `owner_left_secs` inside one tool call (a permission prompt, a question to the user)
  sends no hook meanwhile, so it is marked `ended` too. (Servers before this change left
  such an agent `running`.)
  `said`, optional, is the last message of the turn that just ended: the hooks send it
  with the `Stop` update. It is advisory and never fails the update: the server removes
  terminal escape sequences and control characters other than line breaks and tabs, trims
  it, cuts it to 4,000 characters (`SAID_CHARS_MAX`, ending with `…`) and drops it when
  nothing is left (`turn_ending` in board-proto, which the hooks apply first). While the
  agent holds a live coordinator lease, or is a live judge of a board, it is kept, once
  per such board, as a `[Said]` for the user (see "Updates"), and the update records an `agent_state_changed` event even when
  the state did not change; otherwise it is dropped. The newest 50 per agent and board are
  kept.
- `GET /v1/agents/{id}/handoff` → `[Handoff]` or 404. `last_change_at` is when the agent
  last made a change the handoff must cover, on the same clock as `updated_at`: after the
  handoff, its newest claim, release, take or completion of a task, its newest update of a
  task that is or was its own, or its newest file touch when that came more than 30
  minutes after the handoff. A handoff older than it is stale. The agent's notes, new
  tasks, leases, asks and updates of other tasks, and file touches within those 30
  minutes, do not count: they leave the handoff as true as it was. Without such a change
  after the handoff it equals `updated_at`.
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
  `usage_stop_at` (1 to 100, default 90; 400 `invalid` otherwise) is the usage percent at
  which the board starts no new task (see "Usage"). Raising it is user only (403
  `forbidden` otherwise); anyone may restate or lower it. A `Board` from a server that
  predates it reads as 90. `board board update <slug> --usage-stop-at N` sets it, and
  `board boards` shows ` · new work stops at N% usage` when it is not 90.
  `until` (optional future RFC 3339 timestamp; 400 `invalid` if not in the future or sent
  without `parallel_tasks` or `usage_stop_at`) makes the `parallel_tasks` and/or
  `usage_stop_at` change temporary: the server stores the earlier baseline and expiry on
  the board (`parallel_tasks_until`, `parallel_tasks_prev`, `usage_stop_at_until`,
  `usage_stop_at_prev`, omitted when permanent). Extending an already-timed limit keeps the
  original baseline; updating a limit without `until` clears its timer. While a timed raise
  is active above its baseline, a non-user caller cannot make it permanent or extend its
  `until` past the approved expiry (403 `forbidden`). The server's minute
  sweep restores expired limits, updates live worker write locks when `parallel_tasks`
  reverts, and records a `board_updated` event with actor `server`, `expired: true` and
  `restored`. `board status` and `board boards` show `(until YYYY-MM-DD HH:MMZ, then <prev>)`
  next to timed limits; the CLI accepts `--until <time>` (RFC 3339, `YYYY-MM-DD` or a
  duration ahead such as `2h` or `1d`) or `--until-reset` (from the current usage reading's
  `resets_at`).
  `verdict_judges` (default `true`) controls whether a home device daemon starts automatic
  verdict judges for pending proposals on the board while autopilot is off, and whether the
  server grants lease-less verdict judge registration and rights there (see "Daemon
  dispatch"). Anyone may toggle it (`board board update <slug> --verdict-judges
  <true|false>`, recording `board_updated` with `fields: ["verdict_judges"]`). A `Board`
  from a server that predates it reads as `true`, and `UpdateBoard` leaves the field out
  unless set; the CLI checks the answer and refuses if an older server ignored it.
  `board boards` and `board status` show ` · verdict judges off` when it is `false`.
  `goal_review_every_secs` (`0` to disable, or 60 to 604,800 seconds, i.e. 1 minute to 7 days;
  `null` clears it; 400 `invalid` when out of range) sets the minimum interval between goal
  reviews on the board. Anyone may set, raise, lower, disable or clear it (`board board update
  <slug> --goal-review-every <duration|0|none>`, accepting suffixes `s`, `m`, `h`, `d`, `0`/`off`
  or `none`, recording `board_updated` with `fields: ["goal_review_every_secs"]`). When `0` (or
  when `kind` is `inbox`), `RunKind::Goal` is never dispatched for the board. The server's
  `feedback_board` (`board-feedback`) defaults to `kind: "inbox"` and `goal_review_every_secs: 0`
  (backfilled at migration `0037`). Other project boards default to `null` (immediate goal review
  once work finishes and the board rests). A `Board` from a server that predates it reads as
  `null`, and `UpdateBoard` leaves the field out unless set; the CLI checks the answer and refuses
  if an older server ignored it. `board boards` and `board status` show ` · goal review off` when
  `0` or ` · goal review every <duration>` when positive.
  `kind` (`[BoardKind]`: `project` default, or `inbox`; open enum) distinguishes normal project
  boards from triage/inbox boards (such as `board-feedback`). Inbox boards never run `Goal`
  reviews (`board board update <slug> --kind project|inbox`, ` · inbox` in `board boards`).
  `curate_every_secs` (`0` to disable, or 3,600 to 2,592,000 seconds, i.e. 1 hour to 30 days;
  `null` clears it; 400 `invalid` when out of range) sets the per-board cadence for periodic
  backlog curation (`RunKind::Curate`; `board board update <slug> --curate-every <duration|0|none>`).
  `dispatch` (`[Dispatch]`) is always `daemon`: the dispatch agent of the device holding
  the coordinator lease runs the board's autopilot (see "Daemon dispatch"). The server
  sets it on every new board and drops `dispatch` from an update: `thread`, the
  coordinator-thread dispatch that was removed, is ignored, so an older client's update
  still applies its other fields.
  No client reads it, and no command sets or shows it. A `Board` from a server that
  predates the field reads as `thread`, and a daemon dispatches that board all the same.
  An update that changes `status` adds the new one to the `board_updated` payload
  (`{"fields": ["status"], "status": "paused"}`); the daemon dispatches the board again when it
  reads `active` (see `DESIGN.md`, "Pausing work"). The server's feedback board
  (`defaults.feedback_board`) is never paused: the server drops `status: paused` for it,
  for the user too, applies the rest of the update and answers `200` with the board
  still `active` (closing it is allowed). The minute sweep sets a feedback board left
  `paused` by an older server back to `active`, recorded as `board_updated` by `server`.
  The user-only changes (raising `parallel_tasks` or `usage_stop_at`, turning `temporary`
  on) have two routes for an agent, which each 403 names in `next`; see "Approved board
  changes" below.
  `board pause [<slug>…]` and `board resume [<slug>…]` set every active (or paused) board,
  or those named, with this endpoint; `--json` prints the changed `[Board]` list. A board
  the server kept active is left out of that list, and the text lists it apart, as does
  `board board update --status paused`.
- `POST /v1/boards/{slug}/join` `[JoinBoard]` → `[Board]`.
- `POST /v1/boards/{slug}/coordinator` `[ClaimCoordinator]` → `[Board]`; 409
  `claim_refused` when another device holds the coordinator lease (45 minutes, renewed by
  the daemon and by the coordinator's heartbeats). The device claims first, starts the
  thread, then claims again with the thread's `agent` id, which makes that agent's role
  `coordinator`. A device names only its own dispatch agent as `agent` (403 `forbidden`
  for `dispatch:<another device>`). `DELETE` releases it; only the holding device or the user may. Both emit
  `board_updated` with `coordinator` and the lease's `device` in the payload.
- `POST /v1/boards/{slug}/judges` `[RegisterJudge]` → `[Judge]` and
  `GET /v1/boards/{slug}/judges` → `[Judge]` list: see "Daemon dispatch".
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
  `next`, `blockers`, `question`, `option`, `answer`, `q`, `signature`, `reason`, `goal` or
  `summary` (`board_proto::CLIENT_CHECKED_FIELDS`), so a record carries no text of the client's
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
server's notices are not activity: a `task_updated` with actor `server` and empty `fields`
(a quiet review, an owner away, a passed `not_before`) repeats, or comes, while nobody
uses the board. Time the server was down is not idle time: the server records every
minute, and at shutdown, that it is alive, and takes each gap between that and its next
start out of the period. A restart does not start the period again. When that record
cannot be written, no board is swept until it can. Nor is a board idle while one of its tasks that is not done blocks an
unfinished task on another board. Each board is swept in its own transaction; one that
fails is logged and tried again on the next sweep.

- An `active` temporary board idle for `temporary_close_after_secs` is closed:
  `board_updated` with `fields: ["status"]` and actor `server`.
- A `paused` temporary board is never closed or deleted by the sweep, however long the pause
  lasts: a pause waits on the user. Resuming it is a change, so its period starts then.
- A `closed` temporary board idle for `temporary_delete_after_secs` is deleted as by
  `DELETE` above, with actor `server`. Closing is an event, so deletion comes that long after
  the board was closed at the earliest.

Reopening a closed board, or any change on it, starts the period again.

### Approved board changes

Three board settings are user only: raising `parallel_tasks`, raising `usage_stop_at`, and
turning `temporary` on. A device token gets 403 `forbidden` for them. An agent gets such a
change applied in one of two ways, and in no other. Each use leaves a `board_updated`
event, which the web view's event list shows with its payload.

Policy: an agent uses a route only for a change the user approved specifically, that
setting with that value on that board. A general go-ahead, an older approval or an
approval for another board is not one. An agent that has no such approval asks (route 1).

1. **Approval through an ask.** `[CreateAsk]` takes `change` (`[BoardChange]`:
   `parallel_tasks`, `usage_stop_at`, `temporary`, optional future `until` for
   `parallel_tasks`/`usage_stop_at`; at least one setting, in range, and no other
   field, else 400 `invalid`). The change is for the ask's board. The server writes the
   approving option itself, `Apply: parallel_tasks=2` (the settings as `name=value`,
   joined by `, `, with ` until YYYY-MM-DD HH:MMZ` when timed), and puts it first in `options`, so the user reads what the answer
   applies whatever the question says; the agent may add at most 7 options of its own.
   The `[Ask]` carries `change`. When the user answers with exactly that option (web,
   Telegram button or `board answer`), the server applies the change as the user in the
   same transaction as the answer. No agent step follows. An answer typed as that exact
   text counts as the option; spaces around it are ignored. Any other answer applies
   nothing. An approving answer never lowers `parallel_tasks` or `usage_stop_at` below
   the board's value at that time (leaving that setting alone and naming it in
   `not_lowered`), and never applies a timed limit whose `until` has already passed
   (leaving it alone and setting `expired_before_answer: true` in the event). The agent's own options may not start with `Apply:`. The event: actor `user`, payload `{"fields": [...],
   "parallel_tasks": 2, "approved": "ask", "ask": <id>, "agent": "<the asker>"}`.
   `ask_user` (MCP) takes `change`; `board ask` takes `--set-parallel-tasks N`,
   `--set-usage-stop-at N` and `--set-temporary true|false`. Both say that the first option
   applies the change. A server that predates `change` drops it, and both then warn that
   no answer applies it.
2. **Approval in chat.** `POST /v1/boards/{slug}/approved` `[ApprovedBoardChange]` →
   `[Board]`: `said`, the user's words that approve the change (required, at most 500
   characters), and `change` (`[BoardChange]`, as above, including optional `until`). User only: a device token gets
   403 `forbidden`, whose `next` names both routes. Unknown fields, in the body or in
   `change`, are refused (400 `invalid`), so no other board setting and no other act
   passes through it. `X-Board-Agent` is required and must name a registered agent (400 `invalid`, 404
   `not_found`), so the event always names who applied it. The event: actor that agent,
   payload `{"fields": [...], "parallel_tasks": 2, "approved": "said", "said": "<the
   words>", "device": "<the token's device>"}` (plus `"until"` when timed). It is recorded also when the board already
   had the values (`fields` is then empty); the board itself is then not written.
   `board approved board update <slug> [--parallel-tasks N] [--usage-stop-at N]
   [--temporary true|false] [--until <time> | --until-reset] --said "<the user's words>"`
   calls it.
   For lowering or turning off the account's weekly usage reserve, `POST /v1/usage/approved`
   `[ApprovedUsageReserve]` `{said, reserve, until?}` → `[UsageReserve]` applies the same
   user-token, registered-agent and `deny_unknown_fields` rules, records a
   `usage_reserve_updated` event (`provider`, `account`, `from`, `to`, `by_user`, optional
   `until`/`previous`, `approved: "said"`, `said`, `device`), and is called by
   `board approved usage --reserve <n|off> [--until <time> | --until-reset] --said "<the
   user's words>"`.
   Both commands send the token of the user's login on the
   device, not the session's: the login in the user home, `$BOARD_USER_HOME`, else the
   default home's path with `-user` added (`~/.local/share/agent-board-user`), never
   `BOARD_TOKEN`. The user sets that login up once, with a token created with `--user`
   (`BOARD_HOME=<user home> board login`). Where the user home holds no login, the command
   refuses before any request and names route 1.

### Tasks

- `GET /v1/tasks?board=&status=&owner=&ready=&proposal=&mine=&parent=&live=&closed=&q=&since=&after=&limit=` â
  `[Task]` list in list order: by priority, then the ranked tasks by `rank` (lower first),
  then the tasks without a rank by id number, then board. `mine=true` uses the `X-Board-Agent` header.
  `icebox=true` lists only cooled Icebox tasks (`cooled_at IS NOT NULL`); omitting `icebox` or
  passing `icebox=false` excludes cooled Icebox tasks from listings and from `ready=true`.
  Each `Task` also carries additive v2 backlog fields (all optional / defaulted for `/v1`
  compatibility): `origin` (`[TaskOrigin]`: `user | feedback | agent`), `size` (`[TaskSize]`:
  `xs | s | m | l`), `cooled_at` (RFC 3339 timestamp while parked in the Icebox on `status: open`),
  `ready_at` (RFC 3339 timestamp when the task became ready), `report_count` (server-computed
  linked feedback reports + recurrences), and `effective_priority` (server-computed blocker
  priority inheritance: `min(priority, min_dependent_priority)` across open/active non-cooled
  dependents).
  `parent=<task id>` lists only that task's direct children, not their children; it
  combines with every other filter, and a task without children, or one that does not
  exist, gives an empty list. A malformed id is 400 `invalid`. Servers that predate
  `parent` ignore it and list every task, up to the limit, which may leave children out;
  the CLI and MCP then refuse and point to `board show <id>`, whose detail lists every
  child. Every `Task` has `child_count`, how many tasks name it as parent whatever
  their status; it is absent when none does, and from servers that predate it. The counts
  of a list come from one query, along the parent index. A `Task` with an owner has
  `owner_handle`, the owner's short handle (such as `t3:7f3a`), accepted wherever an
  agent id is; it is absent without an owner, and from servers that predate it.
  `proposal=true` lists only
  pending proposals, `false` leaves them out (see "Pending proposals" below).
  `closed=false` leaves out the `done` and `dropped` tasks, `true` lists only those; it
  combines with every other filter, so `status=done&closed=false` is an empty list.
  Servers that predate `closed` ignore it and list tasks of every status.
  `live=true` lists only the feedback items a new report with their signature would add
  to, whatever their status, by the rule reports follow (see "Feedback" below): the newest
  item with each signature, when it is unresolved and not done or dropped, declined,
  routed to a task that is not done or dropped, or a duplicate whose chain of originals
  (at most 8 links) ends at such an item or at a plain task that is not done or dropped.
  Every item of a live chain is listed. `live=false` lists every other task. It combines
  with every other filter, and `limit` counts the tasks it keeps. `live=true` with a
  `board` that holds no feedback items is 400 `invalid`, whose hint names the feedback
  board; without `board` it lists the items wherever they are. Servers that predate `live`
  ignore it and list every task, so upgrade the server before relying on it. The CLI and
  MCP catch most such servers: they refuse, saying to upgrade it, when the list holds a
  task that can never be live (not a feedback item, forwarded, fixed, or closed
  unresolved). An old server that has only live-looking items passes that check.
  `q=<words>` is a text search: it lists only the tasks that hold every word, split at
  white space (at most 8 words and 200 characters), in their id, title, body, acceptance
  checks, summary, blocked reason or notes. The best match comes first: a word in the id
  or title counts above one in the task's own text, which counts above one in its notes;
  equal matches keep the list's order. A word matches in any letter case, in its stemmed
  form (`deploy` finds `deployed`, and the reverse) and as the start of a longer word
  (`snap` finds `snapshot`), not in the middle of one. Text without spaces between its
  words, such as Chinese or Japanese, matches only from the start of a run. A task id
  is searched whole: `factory-12` finds that task and the tasks that name it, not
  `factory-120`; the parts of an id, such as a board's slug, match in the text only. The parts of a word, such as
  `mcp` and `rs` of `mcp.rs`, must stand in a row. Signs are no part of a word and query
  syntax is plain text; a `q` without a letter or digit finds nothing.
  Each `Task` of a search has `snippet` when a word is in its own text or notes: at most
  200 characters around the words found, on one line, with each found word in `[` and
  `]`. It is absent when the words are in the id or title only.
  When no task holds every word, the search lists the tasks that hold at least one, and
  each of them has `any_word: true`.
  `q` combines with every other filter. With `after`, the cursor's place is its rank in
  the same search; a task that the search does not list is 400 `invalid`.
  A blank `q` is no search; a too long one is 400 `invalid`. Servers that predate `q`
  ignore it and list every task; servers that predate the ranking match a word anywhere
  in a word, list by priority and send no `snippet`.
  `since=<point>` lists only the tasks changed after a point: digits alone are an event
  sequence number (`seq` of a snapshot or event, `next_since` of `GET /v1/events`), and
  anything else is an RFC 3339 time. A task changes with an event that names it: its
  creation, an update, a claim, a note, an ask or a completion. The server's own notices
  about a task (a `task_updated` event with no `fields`) and the lease events of a task
  are not changes. The point itself is not included. Events are kept 365 days, so an older point reads no further back. It
  combines with every other filter. A blank `since` is no filter; a value that is neither
  form is 400 `invalid`. Servers that predate `since` ignore it and list every task.
  `after=<task id>` continues a list: it lists only the tasks that follow that task in
  the list's order, so a list cut at its limit goes on with `after=<id of its last
  task>` and the same filters. The task's place is read at the request, by its priority
  now, and it need not pass the other filters. A task that does not exist is 404
  `not_found`, a malformed id 400 `invalid`. Servers that predate `after` ignore it and
  list from the start; the CLI and MCP then refuse, since that list holds the cursor
  task. `limit` defaults to 200, at most 1,000, and a smaller
  value counts as 1; the list does not say when the limit cut it, so the CLI and MCP ask
  for one task more than their limit and say so in their text, naming the task to
  continue after. The MCP `task_list` asks
  for 25 tasks when the call names no `limit`, since its text goes into an agent's
  context; the CLI keeps the server's default. The MCP `task_list` also narrows a plain
  call to open work on one board, in the client:
  - It sends `board=<the caller's board>` (the one board the caller joined, else the
    one active board of its repository) unless the call names `board`,
    `all_boards=true`, `mine`, `owner`, `parent` or `live`. When no single board covers
    the repository, it lists every board.
  - It sends `closed=false` unless the call names `include_closed=true`, `status`,
    `parent`, `live` or `q`: a search finds done work too, such as an earlier report of
    the same problem. `include_closed` differs from the wire's `closed=true`, which lists
    only the closed tasks.
  - A list that `closed=false` left empty says so in its text. When a server that
    predates `closed` cut its list, the client refuses the list instead of trimming it.
  - `all_boards=true` with `include_closed=true` gives the list of every board and every status
    back. `board` with `all_boards` is refused before sending.
  - A list that names no board and no parent ends its text with
    `boards listed: <slugs>; name a board to narrow it`.
- `POST /v1/tasks` `[CreateTask]` → `[Task]` with status `open`. An optional
  `not_before` (RFC 3339, kept to the millisecond) holds the task: it is not `ready` until
  the server's clock passes it, and a time already passed is ignored. Optional `origin`
  (`user | feedback | agent`; defaults to `user` when created with a user token or `agent == "user"`,
  else `agent`) and `size` (`xs | s | m | l`) annotate the task. Optional rank placement (mutually
  exclusive): `rank` (whole number), `rank_top: true` (places at rank `1` and shifts existing
  ranked tasks in `(board, priority)` down), `before: <task id>`, or `after: <task id>` (places
  adjacent to an open/active anchor in the same board and priority). When `origin == user` and no
  explicit rank placement, proposal body, or `claim: true` is sent, the server automatically places
  the task at rank `1` (shifting existing ranked tasks in that priority level down). On `PATCH
  /v1/tasks/{id}`, `[UpdateTask]` additionally accepts optional `size` (`xs | s | m | l | null`),
  `cooled: true | false` (cools an `open` task into the Icebox or revives it with fresh `ready_at`;
  leaving `open` clears `cooled_at`), and `before` / `after` (mutually exclusive with `rank`).
  When feedback routes or links a repeat report to a cooled Icebox task, the server automatically
  revives it (`cooled_at = null`, `ready_at = now`) and records a `task_updated` event. The `task_created`
  event's payload has `title`, `priority` and, when set, `parent`. While the new task is
  neither done nor dropped, an agent with an unfinished task anywhere above it, other than
  its creator, sees it in its snapshot alerts (`<agent> filed <ids> under your task <id>`).
  - `claim: true` creates the task in `doing` owned by `agent`, in the same transaction, so
    it is never `ready` and the dispatcher starts no worker for it: an agent that files work it
    starts at once no longer races the dispatcher for it. It is checked as a claim is,
    before anything is written: 409 `claim_refused` at the caller's `doing` limit (the
    error names the task in `doing` and says to move it to `review` or `blocked` first), and
    400 `invalid_transition` while a blocker is not done (a blocker in `review` excepted as
    for a claim, below) or `not_before` is ahead. A refused request creates nothing and uses no task number. The `task_created` payload then also
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
- `GET /v1/tasks/{id}` → `[TaskDetail]`. `children` lists the ids of the tasks that name it
  as parent, by id number, and `child_tasks` the same tasks in the same order as
  `{id, title, status, ready, owner, owner_stale}`, so a parent's view needs no request
  per child.
  `child_tasks` is absent when there are none, and from servers that predate it. `history`
  is the task's newest 200 events, oldest first; when it was cut, it does not start with the `task_created` event. `handles` maps
  each registered agent the detail names (owner, creator, note and evidence authors,
  history actors, feedback resolver) to its short handle when that differs from its id;
  it is absent when none does, and `task_show` names agents by these handles.
- `PATCH /v1/tasks/{id}` `[UpdateTask]` → `[Task]`. `not_before` absent leaves it as it
  is, a time sets it, and `null` (or a time already passed) clears it.
- Optional scheduling and routing fields on `[Task]`, `[CreateTask]` and `[UpdateTask]`
  (all omitted on the wire when unset/empty and defaulted when absent from older peers):
  `repo` (origin URL or normalized key, normalized via `normalize_repo`), `weight`
  (`light`, `standard` or `heavy`; `unknown` is refused with 400 `invalid`),
  `resource_units` (1 to 64), `requires` (up to 20 capability tokens), `tolerations` (up
  to 20 `[TaintToleration]` `{key, value?, effect?}` entries; `unknown` effect is 400
  `invalid`), `conflict_keys` (up to 20 `[ConflictKeySpec]` `{key, max}` entries, unique
  keys, `max` 1 to 64), `affinity` (`[TaskAffinity]` `{prefer_device?, prefer_warm_repo?,
  preferred_devices?}`, up to 10 preferred devices), and `preferred_runner` (runner kind
  token, up to 40 characters). In `[UpdateTask]`, nullable fields (`repo`, `weight`,
  `resource_units`, `affinity`, `preferred_runner`) leave the stored value alone when
  absent, clear it on `null`, and set it otherwise; list fields (`requires`, `tolerations`,
  `conflict_keys`) leave the stored list alone when absent and replace it (clearing on
  `[]`) when present. Each changed field is named in the `task_updated` event's `fields`.
- `rank` is a task's place inside its priority level: a whole number, lower first. A task
  without one follows every ranked task of its level, by id number. In `[UpdateTask]`,
  `rank` absent leaves it as it is, a number sets it and `null` clears it. A changed
  `priority` clears it, unless the same request sets one. `rank` is absent from a `Task`
  without one, and from servers that predate it. A change records `task_updated` with
  `"rank"` in `fields` and the new `rank` (or `null`) in the payload. A rank places a task
  and changes nothing of it: a request that changes the rank alone keeps the task's
  `version` and `updated_at`, and its event alerts nobody and leaves a handoff current.
  A task that is done or dropped holds no rank; closing one clears it.
- `POST /v1/boards/{slug}/rank` `[RankTasks]` → `[RankedTasks]` orders many tasks in one
  call. `tasks` holds at most 500 task ids of the board, each once, first to start first.
  They get the ranks 1, 2, 3 and so on in that order, counted per priority level, so one
  list may hold several levels. A task that is not listed keeps its rank, unless
  `clear_others` is true: then every other task of the board loses its rank. Each task
  whose rank changes gets a `task_updated` event as above. The answer holds the listed
  tasks in the order sent. A task listed twice, a task of another board, a done or dropped
  task (the error names each) or more than 500 tasks is 400 `invalid`, an unknown task 404;
  a refused call ranks nothing.
  `board rank <id> <id> ... [--clear-others]` sends it, on the board of the first id, and
  `board update <id> --rank <n|none>` or `task_update` with `rank` sets one task's.
- `reason` in `[UpdateTask]` says why the task is dropped: it applies only with `status:
  dropped` (400 `invalid` otherwise), at most `note_chars` characters, and a blank one counts
  as absent. The server keeps it as a `decision` note by the caller (with its `note_added`
  event), also when the task was already `dropped` and nothing else changes. It is required when the caller drops, under the rule below, the task of an owner
  that has left (400 `invalid`, `next` showing how to send it). That `task_updated` event
  carries `left_owner` (the owner's id). `reason` is sent only when set; a server that
  predates it ignores it and keeps no note.
- `owner` in `[UpdateTask]` hands the task over: the named agent owns it from then on, so
  its `task_done` succeeds and the previous owner's is refused. The owner is worked out
  after the request's status change (`open` clears it, `doing` sets the caller), and
  `owner` then replaces it; naming the owner that leaves changes nothing. Only the task's
  owner, an agent standing in for an owner whose session ended (see "Standing in for an
  ended owner" below), the board's coordinator (or a live judge of the board, see "Daemon
  dispatch") or the user may hand a task over (403 `forbidden`; another agent takes a stale
  owner's task first with `/take`). The status change is checked
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
  dispatches the task if it is then ready. Setting a new `not_before` announces it
  again once that passes. `not_before` is absent from a `Task` without one.
- Within a minute of a task in `review` passing `review_quiet_secs` with no note from its
  owner, the server records one `task_updated` event by actor `server` with payload
  `{"fields": [], "quiet_review": <time>, "in_review_since": <time>}`: `quiet_review` is the
  owner's last note or the move to `review`, whichever is later, and `in_review_since` the
  move. The task is unchanged. While it stays quiet the event repeats every
  `review_quiet_secs`; a note from the owner or a new move to `review` restarts the clock.
  The snapshot shows it as an alert to the owner, or to the board's coordinator when the
  owner has ended or is not live. When the owner is stale or its session has ended (as a take
  requires), and the board is active with neither a coordinator nor daemon dispatch, the
  alert goes to the task's creator (not a feedback item's reporter) and the threads that
  delegated its owner, those that are live; when none is, to the agents registered on one
  of the board's repositories, once the owner is stale. It then names the task and the action: check its PR, and
  once it merged, `task_claim <id> take=true`, then `task_done <id>`; if it will not merge,
  drop it with a reason.
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
  one seen again starts a new one; one the server later marks `ended` as unseen (see
  "Agents") keeps that start. The snapshot shows it as an alert while the task is
  still in `doing` and still that owner's: to the owner, and to the board's coordinator
  when the owner is no T3 thread or has been away at least twice `doing_idle_secs`.
  `board watch` on the owner's device wakes a T3 owner. It wakes the board's coordinator
  instead when the owner is no T3 thread, or when the owner's thread is gone and the
  coordinator runs on the owner's device. Recent activity shows it as `found <agent> away
  from <task>`.
- `POST /v1/tasks/{id}/claim` `[TaskAction]` → `[Task]` with status `doing` and the caller
  as owner. Refused with 409 `claim_refused` when a live agent owns it or the caller is at
  its `doing` limit, and with 400 `invalid_transition` while a blocker is not done or its
  `not_before` has not passed. A blocker in `review` that is an ancestor of the task (its
  parent or above) or that the caller owns does not hold the claim: the task's work went
  out in that blocker's PR, so its agent claims it, moves it to `review` and notes the PR.
  The task stays out of `ready` all the same, and the refusal's `next` names this rule.
  The same holds for a take, a resume, a move to `doing` and a task created with `claim`.
  A pending proposal is refused the same way, with 400
  `invalid_transition`, until a verdict sets it `auto`. A claim
  is one conditional update in one transaction; exactly one of any concurrent claims wins.
  A task in `review` or `blocked`:
  - owned by the caller or by nobody: the claim resumes it, as `PATCH` with
    `status: doing` would (the same `task_updated` event, not `task_claimed`), after the
    checks above (blockers done, `not_before` passed, the `doing` limit, not an unaccepted
    proposal). A `blocked_reason` that the caller set on its own task is cleared, as
    `status: doing` clears it: the owner judges when its reason no longer holds. It is
    refused with 400 `invalid_transition` while the task may wait on the user: an open ask
    on the task, or a `blocked_reason` that someone else set (the latest `task_updated`
    event that lists the field names who), which covers every reason on nobody's task.
    For a reason, `next` quotes it and says what to do once it no longer holds: for the
    caller's own task, resume it with `task_update <id> status=doing` or close it with
    `task_done`; for nobody's, clear it with `task_update <id> status=open`, then claim
    it. For an ask, `next` names it and says to wait for its answer, or to withdraw it
    (`ask_withdraw`, or `board withdraw`), then claim again.
  - owned by another live agent: 409 `claim_refused` naming the owner. When the caller
    stands in for an owner whose session ended, 400 `invalid_transition` instead, with
    `next` naming `take=true`, and the claim is not counted in `claims_refused`.
  - owned by a stale agent: 400 `invalid_transition`, with `next` naming `take=true`.
- `POST /v1/tasks/{id}/release` `[TaskAction]` → `[Task]` back to `open`, no owner. Owner,
  an agent standing in for an owner whose session ended, board coordinator or user only
  (as status changes below).
- `POST /v1/tasks/{id}/take` `[TaskAction]` → `[Task]`. Only when `owner_stale`, or at once
  when the caller stands in for an owner whose session ended (see "Standing in for an ended
  owner" below), else 409 `claim_refused`; records a `task_taken` event naming the previous
  owner (`from`) and the new `status`, which the stats count in `tasks_taken_stale` (see
  "CLI output for scripts"); a stand-in's take carries `stand_in: true` and is not
  counted there. Taking an `open` task moves it to `doing`,
  so it is refused like a claim while a blocker is not done or `not_before` has not passed.
- `POST /v1/tasks/{id}/done` `[DoneTask]` → `[Task]`. Owner, or an agent standing in for
  an owner whose session ended, only (403 `forbidden` otherwise, also for a task nobody
  owns; for the board's coordinator, once the owner's session has ended, `next` names the
  handover to itself first), from
  `doing`, `review` or `blocked`, else 400 `invalid_transition`. The owner closes its
  `blocked` task directly, also while it has another task in `doing`, since a wait often
  ends off the board; this clears
  `blocked_reason`. A `blocked` task with an open ask or a blocker that is not done is
  refused with 400 `invalid_transition` until the ask is answered or withdrawn and the
  blocker is done or removed, so no ask stays open and no waiting task becomes ready
  early. 400 `evidence_required` without evidence, or with `check` evidence missing
  `exit_code`. The `task_done` event carries the evidence. An optional `summary` (1 to 800
  characters, `SUMMARY_CHARS_MAX`) tells the user what changed and what they should know;
  it is stored on the task (`Task.summary`, absent without one) and listed in "Updates".
  It is not in the event; `task_show` shows it among the text written by other agents.
  Reopening the task clears it. An optional `take: true` closes a task in `review` whose
  owner is stale (`owner_stale`) in the same step. It also closes a task in `review`
  without waiting 24 hours when no live managed run exists on the task or its owner, the
  owner is neither actively running (unless left) nor paused for a future reset nor
  delegating live work, and either the owner's managed `worker` run on the task started
  and reached a terminal state or the task has a standing `review: pass`. On an active board
  that nobody runs
  (no coordinator, and no daemon dispatch with autopilot on), an agent on one of the
  board's repositories also closes one whose owner has left, in any status before `done`
  (see "Left owners" below). The caller becomes the task's owner, a `task_taken` event
  (with the task's status) comes before `task_done`, and the task never moves to `doing`,
  so the caller's `doing` limit does not apply. The evidence must then hold a `pr` or a
  `check` (400 `evidence_required` otherwise). With `take`, the task of an owner that
  is not stale is refused with 409 `claim_refused`, unless that owner has left or the
  `review` task qualifies after managed work as above.
  A stale owner's task in another status than `review` is refused with 400
  `invalid_transition`, since `/take` starts such work and keeps the limit. `take`
  changes nothing for the task's owner or an agent standing in for it. Servers that
  predate it ignore it. (Servers before the left-owner drop closed a left owner's task
  only in `review`, and judged it left after twice `doing_idle_secs`.)
- `POST /v1/tasks/{id}/notes` `[AddNote]` → `[Note]`. Kind `feedback` is still accepted;
  agents report feedback through `POST /v1/feedback` instead.
- A task filed through `POST /v1/feedback` carries `feedback` (see "Feedback"); other tasks
  have none, and the field is absent.

Allowed status changes through `PATCH` (anything else is `invalid_transition`):

| From | To |
|---|---|
| `open` | `blocked`, `dropped` (claiming moves it to `doing`) |
| `doing` | `blocked`, `review`, `open` (same as release), `dropped` |
| `blocked` | `open`, `doing`, `review`, `dropped` |
| `review` | `open`, `doing`, `blocked`, `dropped` |
| `done`, `dropped` | `open` (reopen) |

`done` is reached only through `/done`, from `doing`, `review` or `blocked`. A parked task
moves between `review` and `blocked` directly as what it waits on changes.

Who may change the status: anyone while the task has no owner or is `done` or `dropped`;
otherwise only its owner, the board's coordinator (or a live judge of the board) or the
user (a `--user` token or the browser), else 403 `forbidden`. Another agent takes a stale owner's task first
(`/take`, which is logged); an agent standing in for an owner whose session ended may change
it as its owner may; on a board nobody runs, an agent on one of its repositories may drop the
task of an owner that has left, with a `reason` (see "Left owners" below). Moving a task to `open`, from any status, clears its owner.
Moving it to `doing` makes the caller the owner, within the caller's `doing` limit; when
that changes the owner, the `task_updated` event lists `owner` in `fields` and carries
`owner` and `previous_owner`. Like a claim, a move to `doing` (from `blocked` or
`review`) is refused with 400 `invalid_transition` while a blocker is not done or
`not_before` has not passed (a blocker in `review` excepted as for a claim, for the owner
the request leaves), before the `doing` limit is checked. The check reads the task
as the request leaves it, so the same request may drop those blockers (`blockers`) or clear
`not_before` (`null`); the message names every wait and `next` what to drop. A `blocked`
task nobody owns is refused a move to `review` with 400 `invalid_transition` (`next` says
to resume it first): a task in `review` has an owner to finish it, who brought it through
`doing`.

Standing in for an ended owner: while a task's owner is marked `ended` (its session ended
and it has not acted since, see "Agents"), with no usage-limit reset ahead (`paused_until`
not after now) and no thread it delegated at work (as the idle `doing` notice above
judges), the task's creator (`created_by`; for a feedback item, whose creator is its
reporter, nobody) and the T3 threads that delegated the owner (up its `t3_parent` chain, at
most 8 levels) act as its owner. They may take it at once with `/take` (logged), move or
release it, hand it over and mark it done, without waiting until the owner is stale, so a
parent finishes what its finished child still owns. Taking it or moving it to `doing` makes
the caller the owner, within its `doing` limit; moving it to `review` or `blocked`, or
marking it done, leaves the owner as it is. An owner that acts again is `running`, and the
task is its own again. Others are refused as before, and the refusal's `next` names this
rule. (Servers before this change made them wait until the owner was stale.)

Left owners: an owner has left when its session ended with nothing of it left to come
back, or when its session still counts as `running` and has been unseen for
`owner_left_secs` (an hour by default): it stopped without an end hook. Either way no
usage-limit reset is ahead (`paused_until` not after now) and no thread it delegated is at
work. An `idle` owner waits and has not left. On an active board that nobody runs (no
coordinator, and no daemon dispatch with autopilot on), an agent on one of the board's
repositories may drop such an owner's task in any status before `done` or `dropped`, and
must give `reason` with the drop (see `PATCH` above), or close it with `/done` and `take`
once its work has landed. It changes nothing else of the task: a take or a move to `doing`
starts work and still waits until the owner is stale (`owner_stale_after_secs`, 24 hours).
On a board with a live coordinator or daemon dispatch, the coordinator decides instead.
The refusals for other agents name the rule that applies: an ended owner's refusal says
the session has ended, and on a board nobody runs `next` shows how to drop with a reason
or close once the owner has left.

Pending proposals: a task is one while it is `open`, not `auto`, not a feedback item (whose
body is a reporter's text, triaged instead), and its body starts `Proposal:` after leading
whitespace (`board_proto::is_pending_proposal`, one definition for the server, the daemon,
the CLI and the web view). The rule reads the task as it is now: an edit that sets `auto`
or removes the prefix ends it. `Task.proposal` says so; it is computed when read and absent
from servers that predate it (clients apply the rule to the task's fields instead). A
pending proposal is never `ready` and cannot be claimed. No other path takes an
unaccepted proposal (not `auto`, with the prefix, whatever its status) into `doing`:
`PATCH` to `doing`, from `blocked` say, is refused with 400 `invalid_transition` unless
the same request sets `auto`. `review` and `done` follow `doing` (an unowned `blocked`
task cannot move to `review`, and only the owner marks a task done), so a proposal is
not finished without a verdict. The snapshot lists pending proposals under `proposals`,
apart from ready work, with who filed each (an agent does not review its own); one held by
a `not_before` still ahead is listed under `waiting` instead. A proposal becomes work when
another agent records a `verdict:` note and an accept sets `auto` (`DESIGN.md`,
"Proposals and triage").

`blocked` needs `blocked_reason` or at least one blocker that is not done. A
`blocked_reason` that is not blank is accepted only with status `blocked` (400 `invalid`
otherwise). A blank one (empty or only whitespace) counts as absent, with any status: it
leaves a stored reason as it is, so `blocked` with a blank reason still needs a stored
reason or a blocker that is not done. Every move out of `blocked` clears the reason.
Blockers must exist and must not form a cycle; they may be on any board. A task is not
`ready`, and cannot be claimed, until every blocker is done, wherever it is, except as
the claim above says for a blocker in `review` that is its ancestor or the caller's.
A dropped blocker never counts as done, and the server changes neither the status nor the
blockers of a task when its blocker closes (`DESIGN.md`, "Closed blockers"). A `[Task]`
carries `blockers_closed: true`, computed when read and left out when false, while
blockers hold it though every one is done or dropped: it is `blocked`, or `open` with a
dropped blocker. The owner of a task that lists a blocker gets a snapshot alert from the
blocker's `task_done` event and from the `task_updated` event that drops it (`"to":
"dropped"`), and a full snapshot names up to three tasks with `blockers_closed` that the
agent owns or that have no owner or a stale one, its own first; a task with an open ask,
or with a blocked reason and another owner, is left out. Each alert's action holds the
`task_update` call that clears the blockers. The call also moves a `blocked` task that has
no reason, which could not stay `blocked` (`status=doing` for its owner, `status=open`
with no owner or while `not_before` is ahead), and follows `task_claim <id> take=true`
for a stale owner's task. A dropped blocker's alert goes on to the other choices: name
the task that replaces it, or drop the waiting task. An alert for a blocker that was
reopened before the snapshot is not shown.
`GET /v1/tasks/{id}` lists the tasks waiting on it in `blocks`, from every board, and a
blocker's `task_done` event,
on its own board's feed, wakes the waiting task's board coordinator and the owner of a
task blocked on it (`DESIGN.md`, "Waking it"). Deleting a board removes its tasks' links
both ways: a task on another board that waited on one of them loses that blocker, with a
`task_updated` event on its own board, by the deleting actor, with payload `{"fields":
["blockers"], "blockers_removed": [ids], "board_deleted": slug}` and its `version` raised,
so an open task left with nothing to wait on is announced as ready (a done or dropped task
loses the link without an event). A blocked task's owner is not woken by this and sees the
change in its next snapshot. The temporary-board sweep never does this to a blocker that
is not done (see "Temporary boards").

### Daemon dispatch

A board's autopilot runs from a device daemon, not a long-lived thread (`DESIGN.md`,
"Daemon dispatch"). The server
knows two things for it:

- **The dispatch agent** `dispatch:<device>`: an agent id source of its own, whose native id
  is a device name. Only that device's daemon (or the user) acts as it (see
  "Authentication"), registers it (see "Agents") and names it in the coordinator lease
  (see "Boards"). Holding the lease, it has the coordinator's rights on the board: it hands
  tasks over, moves them, and finishes a task it handed to itself with `/done`. Without the
  lease it is an agent like any other.
- **Judges**: short-lived threads the lease holder starts for one judgment. A board that
  is not on autopilot gives no lease; there the dispatch agent of a home device registers
  verdict judges (below).
  - `POST /v1/boards/{slug}/judges` `[RegisterJudge]` `{agent, kind, tasks?}` → `[Judge]`
    `{board, agent, kind, tasks, registered_by, created_at, expires_at, running}`. Only the
    agent holding the board's coordinator lease may call it, acting as itself in
    `X-Board-Agent` (400 `invalid` without the header; 403 `forbidden` for any other agent,
    a judge included). One more agent may, for `kind` `verdict` only: `dispatch:<device>`
    on an `active` board that is not on autopilot, has `verdict_judges` enabled, and lists
    `<device>` in `home_devices` (403 `forbidden` for another kind, another agent, a paused
    board, or a board with `verdict_judges: false`; 409 `claim_refused` while a judge that
    another home device registered there is live, so one device at a time judges the
    board's proposals). `kind` is `review`, `verdict`, `goal` or `triage`.
    `tasks` names tasks of this board: repeats are dropped, then at most 20 may remain
    (`JUDGE_TASKS_MAX`; 400 `invalid` for more), and a `review` names at least one (400
    `invalid`). An unknown task is 404, a task of another board 400 `invalid`. `agent` is
    an id or a handle, stored as the id. The agent may not be registered yet, since its
    session may still be starting; 400 `invalid` when it is the caller or has ended.
    Registrations that ended or expired are deleted then. Records `board_updated` with
    `fields: ["judges"]`, `judge`, `kind`, `tasks` and `expires_at`.
  - A judge has the coordinator's rights on that board, and only there, while all of these
    hold: its agent has not ended since it was registered (a resumed agent does not get them
    back), at most 2 hours have passed since it was registered (`JUDGE_HOURS_MAX`;
    `expires_at`), and the agent that registered it still holds the board's coordinator
    lease, or, for a verdict judge registered without the lease, the board is still
    `active`, not on autopilot, has `verdict_judges` enabled, and is a home of the
    registering agent's device. Registering
    the same agent again before its registration ended or expired replaces `kind`, `tasks` and `registered_by` and keeps `expires_at`; once it is over, a
    registration starts afresh.
  - `GET /v1/boards/{slug}/judges` → the registrations that give rights now, oldest first.
    `running` is true while the judge's agent is `running` or not registered yet: the
    daemon counts those as the judges at work. A judge whose turn ended (`idle` or
    `paused`) keeps its rights until its agent ends, but is not running.
  - A judge's turn endings are kept as a coordinator's (see "Agents", `said`).

### Leases and touches

- `POST /v1/leases` `[CreateLeases]` → `[LeaseResult]`. Advisory: overlapping leases of live
  peers are returned as `conflicts` and raise alerts; nothing is refused. A glob repeated in
  `globs` is leased once. Each lease in the answer names its repo and carries `created_at`
  (when the lease was first taken; kept across live renewals, replaced with a fresh row when
  re-leasing an expired glob, and absent from servers that predate it). `later_conflicts`
  (omitted when empty or from older servers) lists the ids of conflicts for which every
  overlapping requested glob is already overlapped by an earlier live lease of the caller
  (including live leases from earlier calls). `lease_paths` (MCP) and `board lease` use
  `later_conflicts` and `(created_at, id)` (requiring `created_at` on both leases so older
  servers without it never misclassify reused ids): when the peer holds the earlier lease,
  the output names the earlier holder and task and tells the caller to stop or narrow its
  globs (unless the overlap is only append-only/generated paths or the peer's diff touches
  only disjoint hunks); when the caller holds earlier
  leases covering the overlap, it tells the caller its earlier lease holds and to note the
  later task and go on. With a `task` whose
  board lists repos and not `repo`, the lease is still taken in `repo`, and the answer's
  `warning` (absent otherwise) names the board's repos: peers working the task there get no
  warning from this lease. With a `task` that is `done` or `dropped`, the leases are taken
  without a task and the `warning` says so. Leasing a glob the caller already holds renews
  the same lease; when the live lease named another task, or none, the lease then names no
  task and the `warning` lists those globs, so closing one task never ends a lease another
  still needs. Several warnings are joined with `; `. `lease_paths` (MCP) and `board lease`
  print it as a `warning:` line above the leases. Both take an optional `dir` (`--dir` in
  the CLI) to lease globs in that directory's git checkout instead of the current checkout;
  an unknown path, non-directory, non-repo directory, or git repo without an `origin` remote
  returns an error saying what to do next. When `dir` is omitted and the current directory
  has no `origin` remote, passing `task` falls back to `task.repo` when set, else the task's
  board's sole repo; otherwise (no `task`, or a board with zero or several repos) the client
  errors naming `dir` / `--dir`. Out of scope on `/v1`: checkouts with no `origin` remote
  have no local-only repo key of their own.
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
  paths within the last hour. For `via: "lease"`, each `[Overlap]` also carries optional
  `task` (the peer lease's task, when set) and `your_lease_earlier` (`true` when the caller's
  earliest live lease covering the path predates the peer's, `false` when the peer's lease is
  earlier, absent when the caller holds no lease on the path or from older servers) so the
  edit-hook alert shows which lease is older. 1 to 100 paths, each relative to the repo root, one line of at
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
- `GET /v1/asks?status=open&board=&limit=` → `[Ask]` list, oldest first. `limit` is
  optional: with it, the list holds only the newest `limit` asks that pass the filters (0
  reads as 1); without it, every one. The list does not say that it was cut, so a client
  asks for one ask more than it shows, as `board asks` does: it then says that older ones
  match. A server that predates `limit` sends every ask, so `board asks` counts the older
  ones when more than one is over its limit.
- `GET /v1/asks/{id}` → `[Ask]` in any status (the web view's `/asks/<id>` link).
- `POST /v1/asks/{id}/answer` `[AnswerAsk]` → `[Ask]`. User only; `answered_via` records
  `cli`, `web` or `telegram`. The `ask_answered` event's payload names the ask (`id`) and
  the asking `agent`, and the asker's next alerts include the answer.
  An ask that carries a `change` applies it when the answer is its first option (see
  "Approved board changes").
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
  - `text`: the first line is the title, cut to the title limit (80 characters) with an
    ellipsis; the whole text is the body (at most 4,000 characters).
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
  The item's owner or the user only; 403 `forbidden` otherwise, with the next step. An agent
  that resolves an `open`, unowned item claims it in the same transaction (a `task_claimed`
  event before `task_done`; the doing limit does not apply); a caller with no agent id is
  still refused. `kind` is one of:
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
    The item's reports then count on the original (see `Task.feedback` below). When they
    land on an item other than this one (an original that is a feedback item, or the item
    at the end of its duplicate links), that item announces them as a repeat report does,
    under the same hourly limit: its `feedback` note and `task_updated` event, and the plain
    note on its open routed task. The notes credit this item's latest reporter and quote
    its title; the events' actor is the resolver. A duplicate of a plain task keeps its
    reports and notes that task with later ones.
  - `forwarded`: handed elsewhere (another tracker, the user); `reason` says where.
  - `declined`: not acted on; `reason` says why. It stays live, so repeats keep counting.
    When agents keep reporting it, the server sends it back to triage. The check runs on
    each report to a declined item, and when an item is resolved as its duplicate. It needs
    three things: the item was declined at least `declined_reopen_after_days` ago; it has
    gained at least `declined_reopen_reports` reports since the decline, from any agent,
    on it or on items resolved as its duplicates (and theirs, up to 8 links); and at least
    `declined_reopen_agents` of the agents behind them are new. A report counts when both its
    row and its time come after the decline, so errors that a signal item copied from before the decline do
    not count. A new agent had not reported it before the decline: not on this item, not on
    those duplicates, and not as the creator of one of them.
    The report or duplicate resolve that meets all three moves it to `open`, unowned, with
    the resolution cleared and `version` raised. Reports that met the bar during the wait
    take effect with the next report or duplicate resolve after it. A `finding` note,
    credited to that reporter (on a resolve, the duplicate's latest reporter), starts
    `reopened:` and gives the reports and agents since the decline, how many agents are
    new, the decline time and its reason. The events' actor is the reporter, the server
    for an error signal, or the resolver. It comes with `note_added` and a
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

`reports`, `reporters` and `last_reported_at` include those of the items resolved as the
item's duplicates, and of their duplicates in turn (at most 8 links). The server counts
them when it reads the item and copies nothing: each report stays one row on the item it
landed on, so `stats.feedback.reported` counts it once. Reopening a duplicate takes its
reports back off the original, and resolving it again counts them once. Across items,
`reporters` is ordered by each agent's newest report and keeps the latest 20. On a `signal`
item, `reports` counts API errors, not agents' reports. Each report records which it is,
and the plain note on a routed task says how many of the reports are API errors
(`7 reports (4 of them API errors)`), wherever they landed. A chain of more than 8
duplicate links is refused (400 `invalid`), since its last item would take no reports.

### Snapshots and events

- `GET /v1/snapshot?agent=&kind=full|delta|alerts&since=&format=lines|json|yaml` →
  `[Snapshot]`. Rendered on the server (format in `DESIGN.md`, "Text format"), so every
  client injects the same text. `delta` and `alerts` with nothing relevant return empty
  `text`. Alerts come from events since the cursor. The server also advances the agent's
  cursor to the returned `seq`. One request checks at most 1,000 events that can concern the
  agent for alerts; past that, `seq` is the last one checked and the first alert says how
  many wait for the next request. Alerts read only the last 10,000 events; an alert names
  the older ones skipped. An alert about a peer's edit or lease that overlaps the agent's
  ends with the action `re-read before editing` or `coordinate before editing those files`
  (`EDIT_OVERLAP_ACTION`, `LEASE_OVERLAP_ACTION` in `board-proto`); the hooks pass a
  subagent only the `lines` alerts that end so (lease-vs-lease alerts state which lease is
  older before ` -> ` so older clients' subagent filters keep passing them through). `json` is
  compact, so all three formats share the same size caps.
  `held_by` lists the held boards the agent works for: paused ones, and at the weekly hard
  line active ones too (as their coordinator, a thread it
  delegated, or the owner of a task in `doing`, or one of its delegates); every kind then
  has text. In `lines`, the `hold:` line names the boards and what to do, first after the
  header (a full snapshot's intro comes before it); `yaml` has `hold` with `boards` and
  `action`; `json` has `hold` with the slugs only. Empty and left out otherwise, and by
  servers that predate it, which clients read as not held. `held_since` is when the
  latest of those boards was paused: a board keeps the time while it stays paused. The
  hooks end only a turn that began before it. Left out when not held, and by servers that
  predate it. The weekly hard line (see "Usage") holds every active board the agent works
  for in the same way (as coordinator only while its lease runs), only for an agent with
  runtime `claude`, since the usage is the Claude account's: `held_since` is then
  the later of the line's time and the latest pause, and `held_for_usage` is `true` (left
  out otherwise, and by servers that predate it). A board that stopped running the
  agent as its coordinator thread, while boards had such threads (autopilot off, closed,
  daemon dispatch, its device no longer a home device, or another thread named), holds that agent alone, for as long as the board
  exists: the board is in `held_by` and in `stood_down`
  (`[{"board", "reason"}]`, left out when empty and by servers that predate it), and
  `held_since` is the latest of the stand-down's time and the other holds. Its line reads
  `hold: factory stood down: autopilot was switched off -> this thread no longer
  coordinates the board: children already at work finish on their own. When this turn's
  message is the board's message that this thread is no longer its coordinator, do the
  steps it lists, then end this turn. If the user's own message in this turn asks you to
  go on, …`, one per board after any paused one; `yaml`
  adds `stood_down` with `board`, `reason` and `action` under `hold`. At the weekly hard
  line, the `hold:` line reads `hold:
  factory: weekly usage 94.5% reached 94.5% (the user keeps 5.5% in reserve) -> the
  weekly usage reached the user's reserve. If the user's own message in this turn asks you
  to go on, … In every other turn, …, stop: …`; `yaml` adds `usage` with that reason
  under `hold`, `json` has it as `hold_usage`.
  For a `claude` agent with a board, when a usage reading exists, a `usage:` line follows the
  `hold:` line: in full snapshots always, in `delta` only while the board is over its
  `usage_stop_at` or its pace limit is 0 (it then has text even when nothing else
  changed), never in `alerts`. With a pace (see "Usage"), the line ends
  `· pace: today to 84.8% (allows 4.8), projected 80%, up to 3 tasks at once, reserve
  5.5%` (`one task at a time` within 2 points of the line), and at a limit of 0 it reads
  `week projected 85.3% >= today's line 84.8% -> today's share of the weekly usage is
  spent: start no new task or child thread; finish or park the tasks in doing. The next
  day's share starts 2026-11-10 01:00Z.` `json` and `yaml` add `pace` under `usage`
  with `reserve`, `allowance`, `line`, `projected`, `limit`, `next_day` and, at 0,
  `action`. The `you:` line's limit is the smaller of `parallel_tasks` and the pace
  limit.
  It reads
  `5h 12% · week 74% · this board starts no new task at 90%` (adding `· stale` when the
  reading is past its freshness window), or while over
  `week 92% >= this board's stop at 90% -> usage is high: start no new task or child
  thread; finish or park the tasks in doing. The week window resets 2026-11-15 00:59Z.`
  Percents are rounded down. `json` and `yaml` have `usage` with `five_hour` and `week`
  (each left out when the reading lacks it), `read` (age), `stop_at`, and while over
  `over` with `window` (`5h` or `week`), `percent`, `resets` (when known) and `action`.
  The size caps never cut it.
- `GET /v1/events?since=&board=&limit=` → `[EventsPage]`. With
  `Accept: text/event-stream`, streams events as SSE (`id: <seq>`, `data: <Event JSON>`),
  resuming from `Last-Event-ID`, else from `since`. Events older than 365 days are pruned
  (the newest event always stays), so the events just after an old `since` may be gone.

### Usage

The account's usage, as a device daemon last read it (see `DESIGN.md`, "Usage").

- `PUT /v1/usage` `[ReportUsage]` → `[Usage]`. Device tokens: the reading is stored with
  the caller's device (403 `forbidden` for a caller without one). `five_hour` and
  `seven_day` are each optional `{percent, resets_at?}`, as the usage endpoint gives
  them, and at least one is required; `read_at` is required. 400 `invalid` when both
  windows are missing, a percent is not a number from 0 to 1000, or `read_at` is more
  than 5 minutes ahead of the server's clock or more than 30 minutes behind it (it would
  already be stale). The server keeps
  one reading, the newest by `read_at`: an older report changes nothing, and the answer is
  the reading kept.
- `GET /v1/usage` → `[Usage]` or `null` before any report. `board usage` reads the configured
  `quota_bucket()` from `GET /v1/usage/buckets` (falling back to `GET /v1/usage` when the
  server returns 404/405 or that bucket has no reading yet) and prints it with the boards it
  stops from new work, paused ones included; `board usage --reserve` updates that bucket's
  reserve via `PUT /v1/usage/{provider}/{account}/reserve` (or `PUT /v1/usage/reserve` for
  `anthropic/default` or on 404/405).
- A reading is fresh for 30 minutes after `read_at`. A board is over when a window of a
  fresh reading has `percent` at or above its `usage_stop_at` and its `resets_at`, when
  known, is still ahead.
- `Usage.pace` `[UsagePace]` is present while a reserve is set and the weekly window has
  a `resets_at`; left out otherwise, and by servers that predate it: `reserve`,
  `day_start` (the pace day's start), `anchor` (the day's first weekly percent), `line`
  (the day's line), `projected` (the weekly percent projected 5 minutes ahead) and
  `hard_since` (when the weekly percent reached `100 - reserve`; left out below it).
  Clients apply the full pace (including `projected`) while the reading is fresh and the
  weekly reset is ahead: a board's pace limit is 0 at the hard line or when `projected >=
  line`, 1 when `projected >= line - 2`, and its `parallel_tasks` otherwise. Once a reading is
  stale (>30 minutes old), because weekly usage never decreases before `seven_day.resets_at`,
  clients still hold (`0`) while `resets_at` is ahead if measured `seven_day.percent >= 100 -
  reserve` (until `resets_at`) or if still within the same pace day (`now < day_start + 24h`)
  and measured `seven_day.percent >= line`; stale `projected` burn rate alone never holds.
- `GET /v1/usage/reserve` → `[UsageReserve]` `{reserve, until?, previous?}`, `reserve: null`
  while the pace is off, with `until` and `previous` present while a timed reserve override
  is active (also carried on `[Usage]` and `[UsageBucket]` as `reserve_until` and
  `reserve_prev`).
- `PUT /v1/usage/reserve` `[UsageReserve]` `{reserve, until?}` → `[UsageReserve]`. `reserve`
  is a weekly percent from 0 to below 100 that agents leave to the user, or `null` to turn
  the pace off. Anyone may turn it on or raise it; 403 `forbidden` when an agent lowers it
  or turns it off, which only the user may do (or an agent quoting the user through
  `POST /v1/usage/approved`). 400 `invalid` for a value out of range or a non-future `until`.
  When `until` is set, the server stores the earlier baseline and restores it in its minute
  sweep (recomputing `hard_since` when a weekly reading exists), recording a
  `usage_reserve_updated` event with actor `server`, `expired: true`, `from`, `to` and
  `until`. While a timed lowering or disable is active below its baseline, a non-user caller
  cannot make it permanent or extend its `until` past the approved expiry (403 `forbidden`). Every reserve change (and every `POST /v1/usage/approved` call) records a
  `usage_reserve_updated` event (`board: null`). `board usage` prints the timed end time
  and restore target next to the pace status, and `board usage --reserve <n|off>` accepts
  `--until <time>` or `--until-reset`.
- `POST /v1/usage/approved` `[ApprovedUsageReserve]` `{said, reserve, until?}` →
  `[UsageReserve]`. Applies a user-approved reserve change (see "Approved board changes").
- Multi-provider usage buckets (legacy `/v1/usage`, `/v1/usage/reserve` and
  `/v1/usage/approved` alias the `('anthropic', 'default')` bucket):
  - `GET /v1/usage/buckets` → `[UsageBucket]` list, ordered by `(provider, account)`, each
    carrying `provider`, `account`, optional `five_hour`, `seven_day`, `read_at`, `device`,
    `reserve`, computed `pace`, and optional `reserve_until`/`reserve_prev`.
  - `PUT /v1/usage/{provider}/{account}` `[PutUsageBucket]` → `[UsageBucket]`. Device
    tokens only (403 `forbidden` otherwise). `provider` and `account` are 1 to 64
    characters of `a-z 0-9 . _ -` starting with a letter or digit (400 `invalid`
    otherwise). Validation and newest-`read_at` rules match `PUT /v1/usage`.
  - `PUT /v1/usage/{provider}/{account}/reserve` `[PutUsageBucketReserve]` `{reserve, until?}` →
    `[UsageBucket]`. Same validation, timed expiry, event recording and user-only
    lower/disable rule as `PUT /v1/usage/reserve`.

### Devices (fleet registry)

- `GET /v1/devices` → `[DeviceEntry]` list of non-revoked devices ordered by `name`, with
  admin governance fields (`role`, `taints`, `preference`, `drain`), live heartbeat fields
  (`os`, `arch`, `capacity`, `free_units`, `capabilities`, `runners`, `repos`,
  `protocol_features`, `heartbeat_at`), and computed `status`: `online` when
  `heartbeat_at` is within `device_stale_secs` (default 90s) of the server's clock,
  `stale` when older, and `offline` when the device has never heartbeated.
- `PUT /v1/devices/{name}/heartbeat` `[DeviceHeartbeat]` → `[DeviceEntry]`. Device-self
  only: authorized only for `{name}`'s own bearer token or a `--user` token (403
  `forbidden` for any other device token). Updates only live heartbeat fields (`os`,
  `arch`, `capacity`, `free_units`, `capabilities`, `runners`, `repos` normalized via
  `normalize_repo`, `protocol_features`, and `heartbeat_at` from the server clock). Never
  modifies `role`, `taints`, `preference` or `drain`.

### Runs and coordination

First-class ephemeral execution attempts across the fleet (`docs/SCHEDULER_DESIGN.md`,
Section 3.3).

- `POST /v1/runs` `[CreateRun]` → `[Run]`. Creates or submits a run. A device token may
  only create runs for its own `device` (403 `forbidden` otherwise); `--user` tokens and
  browser users may name any active device (404 `not_found` for unknown or revoked
  devices). Creating or mutating `origin: managed` runs requires the user, a device token
  without `X-Board-Agent`, or that device's own `dispatch:<device>` header (403
  `forbidden` for ordinary worker agents, preventing self-escalation to managed
  judge/merger rights). `kind` is one of `worker`, `review`, `verdict`, `triage`, `goal`, `merge`,
  `housekeeping`, `command` or `interactive`; `origin` defaults to `managed` (or
  `interactive`). `state` defaults to `leased` for `managed` runs and `running` for
  `interactive` runs (allowed initial states: `queued`, `leased`, `starting`, `running`).
  `unknown` on any open enum is 400 `invalid`. If `task_id` is set, `board` defaults to
  the task's board (400 `invalid` if a different `board` is named). Live `managed` runs
  receive a renewable `lease_expires_at` (default `run_lease_ttl_secs`, 180s; customizable
  via `lease_ttl_secs` or `lease_expires_at`, mutually exclusive). Requested
  `conflict_keys` (`[ConflictKeySpec]`) are acquired atomically in `conflict_locks` — if
  any key is already at capacity (against the minimum of existing and requested `max`), the
  entire request rolls back with 409 `claim_refused` naming the contended key and holders.
  Records a `run_created` event.
- `GET /v1/runs?board=&task=&device=&agent=&kind=&origin=&state=&active=&limit=` → `[Run]`
  list, newest first (`created_at DESC, id DESC`), each carrying its currently held
  `conflict_locks`. `active=true` filters non-terminal states (`queued`, `leased`,
  `starting`, `running`, `waiting_external`); `active=false` filters terminal states
  (`succeeded`, `failed`, `preempted`, `orphaned`, `cancelled`). `agent` accepts an agent
  id or short handle. `limit` defaults to 100 (`RUNS_LIMIT_DEFAULT`), clamped to 1..=500
  (`RUNS_LIMIT_MAX`). `unknown` enum filters are 400 `invalid`.
- `GET /v1/runs/{id}` → `[Run]` with its currently held `conflict_locks` (404 `not_found`
  when missing).
- `PATCH /v1/runs/{id}` `[UpdateRun]` → `[Run]`. Authorized for the run's owning device
  token or the user (403 `forbidden` for any other device, and for ordinary worker agents
  attempting to mutate a `managed` run). Enforces the strict state
  machine (400 `invalid_transition` otherwise):
  - `queued` → `leased`, `cancelled`
  - `leased` → `starting`, `running`, `failed`, `orphaned`, `cancelled`
  - `starting` → `running`, `failed`, `orphaned`, `cancelled`
  - `running` ↔ `waiting_external`
  - `running`, `waiting_external` → `succeeded`, `failed`, `preempted`, `orphaned`,
    `cancelled`
  - Terminal states cannot transition, renew leases, or acquire conflict locks (archival
    metadata such as `transcript_uri`, `transcript_sha256`, and `outcome` may still be
    recorded after completion or orphan recovery).
  Entering a terminal state clears `lease_expires_at`, sets `ended_at`, and atomically
  releases every `conflict_locks` row held by the run. State transitions and metadata
  changes emit a durable `run_updated`, `run_cancelled`, or `run_orphaned` event; pure
  lease-renewal heartbeats update `lease_expires_at` without spamming the event log.
- **Lease expiry and sticky wake fallback sweeper**: on each sweep tick (against the
  server's injectable clock), every `managed` run in `leased | starting | running |
  waiting_external` whose `lease_expires_at <= now` transitions atomically to `orphaned`,
  releases all held `conflict_locks`, expires any pending `sticky_wakes` for that run, and
  records a `run_orphaned` Trail Rule event carrying `run_id`, `device`, `worktree_branch`,
  `task`, `cause: "lease_expired"`, `lease_expires_at`, `expired_by_ms`, `released_locks`,
  and `expired_wakes`. Managed `waiting_external` runs whose hosting device was revoked or
  whose heartbeat (falling back to device `created_at`) is stale beyond `sticky_grace_secs`
  (default 600s / 10m) — unless paused for a quota reset (`paused_until > now`) on an
  unrevoked device — are similarly orphaned (`cause: "device_stale_sticky_fallback"` or
  `"device_revoked_sticky_fallback"`), and any remaining pending `sticky_wakes` for
  terminal runs, revoked devices, or stale devices past `sticky_grace_secs` are marked
  expired with a Trail Rule `run_updated` fallback event so recovery becomes immediately
  claimable by another awake device.
- **Sticky wakes**:
  - `GET /v1/devices/{name}/wakes` → `[StickyWake]` list (oldest first) of undelivered,
    unexpired wakes for non-terminal runs routed to `{name}`. Authorized only for `{name}`'s
    device daemon (no `X-Board-Agent` or `dispatch:{name}`) or a `--user` token (403
    `forbidden` for other devices or ordinary worker agents; 404 `not_found` for unknown or
    revoked devices).
  - `POST /v1/wakes/{id}/ack` → `[StickyWake]`. Marks `delivered_at` (idempotent once set).
    Authorized only for the wake's owning device daemon (no `X-Board-Agent` or
    `dispatch:{device}`) or a `--user` token (403 `forbidden` otherwise; 404 `not_found`
    when missing).
- **Live run events and cockpit controls (`run-events-v1`)**:
  - `POST /v1/runs/{id}/events` `[PushRunEvents]` `{events: [NewRunEvent]}` →
    `[RunEventRecord]` list of newly stored events retained after capping. Device-self or
    user auth (the run's owning device daemon, the run's own agent, or the user; 403
    `forbidden` for other devices or unrelated worker agents; 404 `not_found` for unknown
    runs). Accepts up to 200 events per batch (`RUN_EVENTS_BATCH_MAX`), each with positive
    `seq`, open `RunEventKind` (`phase`, `tool`, `note`, `message`, `pr`, `error`,
    `cancelled`, `done`; `unknown` is 400 `invalid`), single-line `summary` up to 300
    characters (`RUN_EVENT_SUMMARY_CHARS_MAX`), optional `at`, and optional compact `meta`
    (aliased `meta_json`, up to 16 KiB). Duplicate `(run_id, seq)` pairs are ignored
    idempotently. Atomically retains at most the latest 200 events per run
    (`RUN_EVENTS_CAP`) inside the same `BEGIN IMMEDIATE` transaction and fans retained newly
    inserted records out to live SSE subscribers after commit.
  - `GET /v1/runs/{id}/events?after_seq=&limit=` → `[RunEventRecord]` list ordered by
    `seq ASC` (`limit` defaults to 200, clamped 1..=200; 404 `not_found` for unknown runs).
    With `Accept: text/event-stream`, opens a gapless SSE stream (`id: <seq>`, `data:
    <RunEventRecord JSON>`) subscribing before reading the backlog, resuming from
    `Last-Event-ID` (400 `invalid` if not an integer) else `after_seq`, with keep-alives and
    clean termination on server shutdown or client disconnect.
  - `POST /v1/runs/{id}/steer` `[SteerRunRequest]` `{message}` → `[SteerRunResponse]`
    `{wake_id, run_id, device}`. Authorized for the user (`--user` token or browser user),
    the run's owning device daemon (no `X-Board-Agent` or `dispatch:{device}`), or (for
    `interactive` runs) the run's own agent on that device (403 `forbidden` otherwise).
    Validates non-empty `message` up to 4,000 characters
    (`RUN_STEER_MESSAGE_CHARS_MAX`), requires an active run (`leased | starting | running |
    waiting_external`, else 400 `invalid_transition`), enqueues a `kind = "steer"` sticky
    wake targeting `run.device`, and emits a Trail Rule `run_steered` event.
  - `POST /v1/runs/{id}/cancel` `[CancelRunRequest]` `{reason?}` → `[CancelRunResponse]`
    `{run_id, device, state, wake_id?}`. Same caller authorization as `steer` (403
    `forbidden` for other devices or unrelated worker agents). Validates optional non-empty
    `reason` up to 500 characters (`RUN_CANCEL_REASON_CHARS_MAX`) and requires a non-terminal
    run (400 `invalid_transition` otherwise). A `queued` run transitions immediately to
    `cancelled` (`wake_id` absent), releasing any `conflict_locks`, expiring pending wakes,
    and recording `run_outcomes`. An active run (`leased | starting | running |
    waiting_external`) enqueues a `kind = "cancel"` sticky wake targeting `run.device` (with
    `wake_id`). Both paths emit a Trail Rule `run_cancelled` event carrying the actor and
    reason.
  - **Retention pruning**: the server sweeper automatically deletes `run_events` rows for
    terminal runs once 7 days have elapsed since `ended_at`, or 1 hour after `transcript_uri`
    is recorded on the run.
- **Atomic pull dispatch (`POST /v1/dispatch/claim`)**:
  - `POST /v1/dispatch/claim` `[DispatchClaimRequest]` → `[DispatchClaimResponse]`
    `{claimed, holds_summary}`. Authorized for the named device's own bearer token (or a
    `--user` token) and for managed-run callers (device daemon without `X-Board-Agent`,
    `dispatch:<device>`, or user; 403 `forbidden` for another device or ordinary worker
    agent; 404 `not_found` for unknown or revoked devices). Draining devices (`drain: true`)
    return empty `claimed` with a `drain` hold summary.
  - Runs the 5-stage pipeline inside a single `BEGIN IMMEDIATE` SQLite transaction:
    1. **Enumerate** across active boards: `worker` (ready `auto` non-proposal, non-feedback,
       non-cooled tasks not blocked by an in-progress task and lacking an active `worker` run),
       `review` (pre-merge tasks in `review` lacking a standing `review: pass` and active review
       run/judge, gated on owner not actively `running` — or `owner_left` — and having a PR URL
       in notes/evidence, a `pr: none` note, or >= 15m in `review`; plus after-merge tasks whose
       standing `review: pass` names `after merge:` checks and already carries the dispatcher's
       `note: merged; waiting for after-merge checks` handoff, except when all listed checks are
       standard `deploy/upgrade.sh --yes` deploys without a prior deterministic fallback finding),
       `merge` (tasks in `review` with a standing `review: pass` — open PR, already merged, or
       `pr: none` — unless already handed off to an LLM after-merge review, plus already-merged
       standard `deploy/upgrade.sh --yes` after-merge tasks routed to `DeterministicRunner` at 0
       LLM cost; cost 1, runner
       `deterministic`, `repo:<repo>:merge` max 1; suppressed for a standing pass that
       already returned `outcome.status = "unclear"` until a newer `review: pass` note or
       newer move to `review`, and cooled down per task with exponential backoff `60s -> 180s ->
       300s` on consecutive generic `deferred`/`noop` polls, `300s` after `deferred` with
       `reason` starting `"checks failed"`, or `120s` after `failed`, cleared early by a newer
       `note_added`/`task_updated` event; `deferred`/`noop` merge runs skip `run_outcomes`),
       `housekeeping`
       (gone-asker answered asks and away-owner `doing` tasks past repeat `idle_doing` +
       grace, cost 1, runner `deterministic`), `verdict` (pending proposals on active boards
       with `verdict_judges: true`, both autopilot and off-autopilot; on active autopilot
       `project` boards with curation enabled — `curate_every_secs != Some(0)` — only fast-lane
       proposals (`effective_priority == p1`, `origin == user`, or `report_count >= 3`) trigger
       immediate `verdict`, while routine `p2`/`p3` agent proposals wait for `curate`; grouped by
       parent, sorted by `(effective_priority, report_count DESC, rank IS NULL, rank, created_at,
       id)` before slicing to 5; non-`p1` batches with `max_report_count < 5` and smaller than
       `judge_batch_min_size` wait up to `judge_batch_debounce_secs`), `triage` (ready feedback
       items on `feedback_board` when autopilot is on, sorted by the same priority + report order
       before slicing to 5, with the same debounce), `curate` (active autopilot `project` boards
       with `curate_every_secs != Some(0)`, 1 per board, `kind_factor = 0.35`, `cost_units = 2`,
       triggered when curatable work exists — at least 1 pending proposal, `>= 10` open uncooled
       tasks, or ranked Up Next Frontier `< 10` with `>= 2` ready tasks — and either daily
       cadence `curate_every_secs.unwrap_or(86_400)` elapsed or frontier-refill is due past a 1h
       cooldown), `goal` (active autopilot `project` boards
       with a non-empty goal, `goal_review_every_secs != 0`, at least one `task_done` newer than
       the last succeeded goal review's start time `COALESCE(started_at, created_at)`, past any
       effective `goal_review_every_secs` cooldown from the last goal review's completion, and
       nothing else active/ready; disabled on `inbox` boards), and explicitly `queued` `command`
       runs.
    2. **Admission**: checks board `active`/`autopilot`, per-provider `(provider, account)`
       `UsageBucket` quota and weekly pace (zero-LLM `merge`, `housekeeping`, and
       deterministic `command` runs bypass LLM quota checks), worker-only `parallel_tasks` cap
       (counting `doing` tasks union unclaimed `leased | starting | running` worker runs;
       never starving judges, merges, or housekeeping), worker-only review-backlog
       backpressure (holding `worker` with `capacity` while a board has at least
       `max(3, parallel_tasks * 2)` tasks in `review`), worker-only leased-file overlap avoidance
       (holding `worker` with `lease_overlap` when likely paths in the task body, acceptance,
       notes, prior `lease_created` globs, or `conflict_keys` overlap an active `doing` task's
       live file leases), judge caps (`JUDGES_AT_ONCE = 2` per
       board and `HOME_VERDICTS_AT_ONCE = 2` across off-autopilot boards), and fleet-wide
       `conflict_keys` capacity.
    3. **Filter**: enforces device `NoSchedule` taints (including control/service-host plane
       isolation unless explicitly tolerated), `requires` capabilities, runner compatibility,
       repo availability (`repos` or `git:cloneable` for runs needing a local checkout), and
       weighted `free_units` fit.
    4. **Strict Economic Score & Backfill**: scores candidates in strict non-overlapping bands:
       `kind`, then `effective_priority` (`P1 > P2 > P3`), then explicit Frontier `rank` (`1..K`)
       strictly above unranked deterministic WSJF (`impact / size_weight`, `report_count`, blocker
       fan-out), with `ready_at` `age` strictly capped at `<= 8.0`, plus rolling 24h `fair_share`,
       warm-repo `locality`, `device_pref` (admin `preference`, task `affinity`, ordered
       `board.home_devices`, and `PreferNoSchedule` soft taint penalty), and `backfill` when a
       smaller job fits `free_units` while a higher-scoring heavier job cannot.
    5. **Atomic Grant**: leases up to `max_claims` (default 1, clamped 1..=4) runs in
       `leased` state (`lease_expires_at = now + run_lease_ttl_secs`), acquires all
       `conflict_keys`, and emits a Trail Rule `run_created` (or `run_updated` for queued
       commands) event carrying `score`, `score_breakdown`, `policy_version`, and `cause`.
  - `supported_kinds` defaults to all managed kinds when empty; `unknown` or `interactive` is
    refused with 400 `invalid`. `quota_bucket` (`{provider, account}`) defaults to
    `("anthropic", "default")`.
- **Dispatcher-agnostic judge/merger rights and standing reviews**: any `agent_id` linked
  to a live `managed` `Run` (`state` in `leased | starting | running | waiting_external`
  with unexpired `lease_expires_at > now`) of kind `review | verdict | triage | goal | curate |
  merge | housekeeping` on a board receives coordinator rights on that board (task
  handovers, status changes, kept turn endings, unseen-sweep protection) without holding a
  per-board `coordinator` lease. Standing `review: pass` verification checks both that a
  task in `review` has a `review: pass` decision note written after its latest move to
  `review` (never `review: fail` or `review: void`, even when the run exits `succeeded`)
  and that the author (not the task's owner) matches a registered `review` `Run` for that
  task on any device or a legacy registered review judge.
- **Automatic interactive `Run` lifecycle**: user-started sessions (T3 Code, standalone
  `claude:`, `codex:`, `opencode:`, `antigravity:`) automatically appear in
  `GET /v1/runs?origin=interactive` via existing `POST /v1/agents` and
  `POST /v1/agents/{id}/state` hook calls with zero extra hook round-trips (see "Agents").
  Claiming, taking, updating, releasing, resolving (feedback), or completing a `doing` task
  keeps the agent's active interactive `Run`'s `task_id` (and `board`) synchronized in the
  same transaction. If a `SessionStart` hook registers an agent slightly before
  `POST /v1/runs`, `PATCH /v1/runs/{id}`, or `POST /v1/dispatch/claim` links or leases that
  `agent_id` on a live `origin: managed` `Run`, the managed run absorbs any missing metadata
  from the auto-upserted `run-interactive-*` placeholder and removes the duplicate
  placeholder atomically.
- **Structured `run_outcomes` telemetry**: whenever any `Run` transitions to a terminal state
  (`succeeded`, `failed`, `preempted`, `orphaned`, `cancelled`)—via `PATCH /v1/runs/{id}`,
  `sweep_expired`, or interactive session end—or updates `outcome` after completion, the
  server atomically upserts its `run_outcomes` row in the same transaction, recording
  `ready_to_leased_secs`, `leased_to_running_secs`, `running_secs`, `conflict_wait_secs`
  (caller-provided or derived from recorded `conflict_key` holds), `weight_unit_minutes`
  (`cost_units * running_secs / 60`), `orphan_count`, `retry_count`, `policy_version`, and
  optional caller-provided `outcome` metrics (`quota_bucket`, `tokens_in`, `cached_tokens`,
  `tokens_out`, `reasoning_tokens`, `warm_cache_hit`, `ci_runs_before_merge`,
  `review_rounds`, `first_pass_merge`). When `warm_cache_hit` is absent from `outcome` but
  `tokens_in > 0` and `cached_tokens` are reported, the server derives
  `warm_cache_hit = (cached_tokens * 2 >= tokens_in)`. When a `merge` run succeeds (outside
  `awaiting_after_merge`/`deferred`/`noop`/`unclear`/`reopened`/`blocked_for_review` outcomes)
  for a task, the server fills missing `review_rounds` as the maximum of succeeded pre-merge
  `review` runs on that task (excluding `after-merge …` runs) and its `review: pass` (without
  `after merge:` lines) / `review: fail` decision notes (ignoring orphaned/cancelled review
  retries), and fills missing `first_pass_merge` as true iff there are zero `review: fail`
  decision notes on the task and `review_rounds <= 1`.
- **Versioned declarative scheduler policy**:
  - `GET /v1/scheduler/policy?history=true|false` → `[SchedulerPolicyResponse]` (flattens the
    active `[SchedulerPolicy]`, plus optional `history` of up to 50 recent versions when
    `history=true`). Seeded with `v1-default` (`w_kind=100`, `w_prio=30`, `w_rank=10`,
    `w_age=16`, `w_fair=12`, `w_loc=8`, `w_pref=1`, `w_backfill=6`, `lease_ttl_secs=180`,
    `sticky_grace_secs=600`). `POST /v1/dispatch/claim` reads the active policy and stamps
    `policy_version` on every leased run.
  - `PUT /v1/scheduler/policy` `[PutSchedulerPolicy]` → `[SchedulerPolicy]`. Creates and
    activates a new immutable policy version (existing version names are rejected with 400
    `invalid`), validating weights (`0..=1000`, finite), `lease_ttl_secs` (`30..=3600`),
    `sticky_grace_secs` (`60..=86400`), and optional `notes` (at most 1,000 characters), and
    records a `scheduler_policy_updated` Trail Rule event. Non-user callers must supply
    non-empty `notes` citing replay/telemetry evidence (400 `invalid` otherwise), and an
    unregistered `X-Board-Agent` header is refused with 403 `forbidden`.
- **Quantitative scheduler telemetry (`GET /v1/stats/scheduler`)**:
  - `GET /v1/stats/scheduler?days=7&board=&policy_version=` → `[SchedulerStats]`. Aggregates
    `run_outcomes`, `devices`, and policy-scoped deduplicated `scheduler_holds` (`quota`,
    `conflict_key`, `capacity`, `taint`, `requires`, rate-limited per `(board, task_id,
    reason, conflict_key, policy_version)` per hour and pruned after `error_retention_days`,
    default 35d) over `days` (1..=365, default 7): `runs_total`, `runs_by_kind`,
    `runs_by_terminal_state`, `orphan_rate`, `first_pass_merge_rate`, `warm_cache_hit_rate`,
    `p50`/`p90` percentiles for `ready_to_leased_secs`, `leased_to_running_secs`,
    `running_secs`, and `conflict_wait_secs`, `total_weight_unit_minutes`,
    `avg_ci_runs_before_merge`, `avg_review_rounds`, optional token rollups
    (`runs_with_tokens`, `tokens_in_total`, `cached_tokens_total`, `tokens_out_total`,
    `reasoning_tokens_total`, `token_cache_hit_rate`, and `tokens_by_kind` mapping each
    `RunKind` to `[KindTokenStats]` `{runs, tokens_in, cached_tokens, tokens_out,
    reasoning_tokens, cache_hit_rate?}`), `per_device` (with live
    online/stale/offline status), `per_board` (with fair-share % and holds),
    `per_policy_version` cohort comparison, `holds_by_reason`, and optional
    `latest_evaluation` (`[SchedulerEvaluation]`, absent until an automated evaluation has
    run). Advertised via `scheduler-telemetry-v1` in `X-Board-Features` and `/healthz`.
- **Automated scheduler evaluation & guardrailed self-tuning (`scheduler-autotune-v1`)**:
  - Configured via optional `[scheduler.autotune]` in `server.toml` (`enabled = true`,
    `mode = "apply" | "propose" | "off"`, `interval_secs = 21600` min `60`,
    `window_days = 7` `1..=90`, `min_runs = 20`, `min_improvement_pct = 5.0`,
    `cooldown_secs = 86400`, `rollback_min_runs = 10`, `rollback_orphan_delta = 0.05`,
    `rollback_p90_ratio = 1.25`). The server sweeper runs evaluations periodically in an
    isolated write transaction using the in-process deterministic replay simulator:
    1. *Auto-rollback watchdog*: if an auto-promoted policy accumulates `>= rollback_min_runs`
       post-promotion `run_outcomes` whose live `orphan_rate` exceeds baseline `+ rollback_orphan_delta`
       or whose live `ready_to_leased` p90 exceeds baseline `* rollback_p90_ratio`, the server
       reactivates `previous_policy_version`, records `decision = rolled_back`, emits a
       `scheduler_policy_updated` Trail Rule event, and files a deduplicated diagnostic signal
       on `feedback_board`.
    2. *Deterministic bounded candidate search*: if fewer than `min_runs` outcomes exist in
       the window, records `skipped_insufficient_data`; otherwise replays bounded candidate
       weight profiles around the active policy with zero regression on `tasks_scheduled`,
       `conflict_blocks`, and `p50_wait_secs` (`<= baseline * 1.05`).
    3. *Guardrailed promotion or proposal*: keeps the current policy when composite
       `p90_wait_secs` + `fair_share_gini` gain is below `min_improvement_pct` or cooldown is
       active; in `apply` mode activates `v<N>-auto-<YYYYMMDD>` and emits
       `scheduler_policy_updated`; in `propose` mode files a structured `Proposal:` task
       (on `agent-board` when active, else `feedback_board`) and records its id in
       `feedback_task_id`.
  - `GET /v1/scheduler/evaluations?limit=20` → `[SchedulerEvaluationsResponse]` (`mode`,
    `enabled`, `interval_secs`, and newest-first `evaluations: Vec<SchedulerEvaluation>`;
    `limit` `1..=200`, default `20`). Authenticated read.
  - `POST /v1/scheduler/evaluations/run` → `[SchedulerEvaluation]`. User/admin only (403
    `forbidden` for non-user device tokens). Runs one evaluation cycle immediately,
    bypassing the `enabled` and `interval_secs` schedule gates while respecting `mode`.
  - Open enums: `[AutotuneMode]` (`off`, `propose`, `apply`, `unknown`) and
    `[AutotuneDecision]` (`keep`, `promoted`, `proposed`, `rolled_back`,
    `skipped_insufficient_data`, `unknown`). Advertised via `scheduler-autotune-v1` in
    `X-Board-Features` and `/healthz`.
- **Automated pathology signals**: the 60-second server sweeper runs isolated pathology
  detection against the injectable clock and files deduplicated `FeedbackSource::Signal`
  items on `feedback_board` (throttled via `pathology_state` to at most one report per
  signature per hour):
  - `signal:scheduler:unschedulable:<task_id>` — ready `auto` task matches zero online
    untainted non-draining devices continuously for `>= unschedulable_after_secs` (default
    3600s).
  - `signal:scheduler:starvation:<board>` — active autopilot board below its `parallel_tasks`
    cap has a schedulable ready `auto` task waiting `>= starvation_after_secs` (default
    21600s) with 0 managed runs while other boards claim runs.
  - `signal:scheduler:orphan_storm:<device>` — device orphaned `>= orphan_storm_count`
    (default 3) runs within a rolling 1-hour window.
  - `signal:scheduler:conflict_hot:<key>` — `conflict_key` held claims for
    `>= conflict_hot_percent` (default 30%) of active hours (across at least 3 distinct
    hours and 3 holds) in a rolling 24-hour window.
  - `signal:runner:spawn_fail:<device>:<runner>` — `(device, runner)` had `>= spawn_fail_count`
    (default 3) fast failed runs (`running_secs <= 30` or never started) within 1 hour.

### Administration (user only)

- `POST /v1/admin/devices` `[CreateDevice]` → `[DeviceToken]` (token shown once).
- `GET /v1/admin/devices` → `[DeviceInfo]` list, never with tokens.
- `PATCH /v1/admin/devices/{name}` `[AdminUpdateDevice]` → `[DeviceEntry]`. User only
  (403 `forbidden` for device tokens). Updates `role` (`control`, `runner`,
  `service_host`; `unknown` is 400 `invalid`), `taints` (up to 20 `[Taint]` entries;
  `unknown` effect is 400 `invalid`), `preference` (-1000 to 1000) and `drain`. Records a
  `device_updated` event naming the actor, device, changed `fields` and resulting values.
  `DELETE /v1/admin/devices/{name}` revokes; a revoked name may be created again (resetting
  its scheduling metadata).
- On the server host, `board admin device <name> [--user] --db <file>` creates a device
  without HTTP (the first user token comes from here), `board admin telegram-pair
  [--replace] --db <file>` prints a one-time Telegram pairing code (see `DEPLOY.md`),
  `board admin backup --db <file> --out <new file>` writes a consistent copy, and
  `board admin replay-schedule --db <file> [--policy <toml|json>] [--days N] [--since T]
  [--json]` opens `<file>` strictly read-only and deterministically replays historical
  tasks/runs under both the active baseline policy and an optional candidate policy,
  printing a comparative table (or `[ReplayComparison]` JSON) of `tasks_scheduled`,
  `p50_wait_secs`, `p90_wait_secs`, `fair_share_gini`, `conflict_blocks`, `backfill_count`,
  and `locality_hits`. Over HTTP with a `--user` token, `board admin device-update <name>`
  calls `PATCH /v1/admin/devices/{name}`, and `board devices` lists `GET /v1/devices`.

### CLI output for scripts

`--json` prints the server's answer as this `board` read it, pretty-printed: fields it does
not know are left out, fields an older server lacks get their defaults, and values of open
enums it does not know read `unknown` (see "Compatibility"). Scripts, such as a weekly
review, may rely on these shapes, which follow `/v1`'s compatibility rules:

- `board stats [--days N] --json` → `[Stats]` from `GET /v1/stats?days=N` (default 7):
  (With `--scheduler [--board <slug>] [--policy-version <v>]`, queries
  `GET /v1/stats/scheduler` and prints `[SchedulerStats]` JSON or a human-readable
  telemetry summary.)
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
  `review` to `open` or `doing` (a move to `blocked` only parks the task on another wait);
  `pr_links` in the evidence of the tasks done;
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
  most 200 tasks; pass `--limit <n>` (up to 1,000) for more. When more tasks match than
  the limit, the text output ends with a line `listed the first <n>; more match: …`
  (`the next <n>` after `--after`) that names the task to continue after; the JSON output
  has no such sign. With `--json`, `--fields id,title,status` keeps only
  those fields of each task; one the task leaves out, such as `summary`, is `null`. A
  name that is no field of `[Task]`, and `--fields` without `--json`, are refused before
  sending. A line names a task's owner by `owner_handle` when the server sends it.
  `--since <point>` sends `since`: an event sequence number as it is, and an RFC 3339
  time, a date (`YYYY-MM-DD`, midnight UTC) or a duration back from now (`30m`, `12h`,
  `2d`) as a time; the MCP `task_list` takes the same forms. `--after <id>` sends
  `after=<id>`, and the cut line names the id to pass: `board ls --limit 50 --after
  factory-62`.
  `--search <words>` (alias `--grep`) sends `q=<words>`, at most 8 words and 200
  characters, refused before sending when longer. The text output shows a task's
  `snippet` on an indented line below it, quoted, and ends with the line `no task holds
  every word of the search; these hold at least one` when the tasks have `any_word`. A server that predates `q` ignores it
  and lists every task, so upgrade the server first.
  `board ls` without `--board` lists the tasks of every board, as before; unless
  `--parent` is given, its text then ends with `boards listed: <slugs>; name a board to
  narrow it`, the boards of the tasks it listed. The JSON output has no such line.
  `--open` sends `closed=false`, which leaves out the done and dropped tasks; it cannot
  go with `--status`. Without it the CLI lists tasks of every status.
  `--parent <id>` sends `parent=<id>`. Without `--json`, a task with children ends its line
  with `children <n>`. `--live` sends `live=true`: `board ls --live --json` lists every
  feedback item that still takes reports, such as declined ones and ones routed to open
  work, which `--status` alone cannot select. Against a server that predates the filter it
  fails with "upgrade it" once the list holds a task that can never be live; upgrade the
  server first.
- `board show <id> --json` → `[TaskDetail]` from `GET /v1/tasks/{id}`. Without `--json`, a
  task with children has a `children <n>: done 5 · doing 1 · …` line, a count per status
  (done, review, doing, blocked, open, dropped; statuses without children left out),
  followed by one indented line per status with its child ids. A task without children
  has no such line. From a server that predates `child_tasks` the line is
  `children <ids>`.
  The text names neither the board (the id holds it) nor the version, and its lines of
  agent-written text carry no `task` key, since the first line names the task.
  `board show <id> <id>…` (MCP `task_show` with `ids` beside `id`) reads several tasks in
  one call, at most 20, each from `GET /v1/tasks/{id}` at the same time, and prints them
  in the order given in a compact form: each task's own lines, with a count of its plain
  notes instead of the notes, then the agent-written text of all of them (bodies,
  acceptance checks, every finding and decision) under one header, each line with its
  `task`. A task that cannot be read is named with the reason and the others are still
  shown; when none can be read the call fails. An id named twice counts once, and one
  task alone is the full form. `--detailed` takes one task. `--json` prints `{tasks,
  not_shown}`: the `[TaskDetail]` list of the tasks that were read, and `{id, reason}`
  for each that was not.
  The text of one task ends with `changes only on the next read: since <n>`, the `seq` of
  the latest event in its `history`. `board show <id> --since <n>` (MCP `task_show` with
  `since`) reads the same `GET /v1/tasks/{id}` and prints only what came after that
  event: the task's line, each later event of its `history` (a status change as `<from>
  to <to>`, with the changed fields' names), the evidence and notes added from the first
  later event on, the present text of each changed field (`body`, `acceptance`,
  `blocked_reason`, and `summary` and `resolution_reason` after a done), and the next
  `since`. A note is later when a later `note_added` event names its id; evidence when
  its time is not before the first later event's. Other changed fields are named only,
  and a field that was cleared has no text line. When no event is
  later it prints one line, `no change since #<n>: <task line>`. `since` also takes an
  RFC 3339 time, a date or a duration back (`2h`). A change of a child or a blocker is
  not an event of the task, so read those tasks themselves. When the `history` starts
  after the point (a newer task, or more than the 200 events a detail holds) the changes
  cannot be told and the whole task is printed, with a line saying so. `since` takes
  one task and no `--detailed`; `--json` prints the whole `[TaskDetail]`.
- `board feedback [--about A] [--activity X] [--source agent|capture] [--signature S] <text>
  --json` → `[FeedbackResult]` from `POST /v1/feedback`; `--about` defaults to `other`.
- `board resolve <id> (--routed <task> | --duplicate <task> | --forwarded <reason> |
  --declined <reason> | --fixed <what changed>) --json` → `[Task]` from `POST /v1/feedback/{id}/resolve`.
- `board runs [--board <slug>] [--task <id>] [--device <name>] [--kind <k>] [--origin
  managed|interactive] [--state <s>] [--active] [--limit N] --json` → `[Run]` list from
  `GET /v1/runs`. Without `--json`, prints one concise line per run (id, origin, kind,
  state, device, runner, cost units, task/board, branch, elapsed/age, and quoted title).
- `board logs <run-id> [-f|--follow] [--after-seq N] [--limit N] --json` → `[RunEventRecord]`
  list from `GET /v1/runs/{id}/events` (with `-f/--follow`, tails live SSE updates and prints
  one `[RunEventRecord]` JSON object per line, stopping on Ctrl-C or once a terminal event
  closes the run). Without `--json`, prints structured event lines (`#<seq> <time> [<kind>]
  <summary>`) and appends `transcript: <uri>` when archived.
- `board steer <run-id> <message> --json` → `[SteerRunResponse]` from
  `POST /v1/runs/{id}/steer`, confirming `wake_id` and owning `device`.
- `board cancel <run-id> [--reason <text>] --json` → `[CancelRunResponse]` from
  `POST /v1/runs/{id}/cancel`, confirming `state` and optional `wake_id`.
- `board attach <run-id> [--dry-run] --json` → attach plan `{run_id, device, local, runner,
  state, session_id?, workdir?, branch?, attach_hint?, t3_link?, ssh_command?,
  checkout_command?, command, commands}` resolved from `GET /v1/runs/{id}` and local
  `$BOARD_HOME/state/daemon.db`. For a local run in an interactive TTY without `--dry-run`,
  opens the T3 thread link, attaches to tmux, or starts a shell in the run's worktree; with
  `--dry-run`, non-TTY, or remote runs, prints the exact actionable local/SSH/git-worktree
  commands cleanly.

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
| Text limits (`title_chars`, `note_chars`, `handoff_now_chars`, `handoff_field_chars`, `question_chars`, `body_chars`) | title 80, note 800, handoff `now` 400 (or `handoff_field_chars` when larger), handoff `next` and `blockers` 300, question 1,500, body 4,000 characters; outside settings, done summary 800, ask option 60 and evidence value 500. A blocked reason takes the note limit, an answer and a board title take the question and title limits |
| Feedback board (`feedback_board`) | `board-feedback`, created on first use |
| Signal: errors, agents, window (`signal_min_reports`, `signal_min_agents`, `signal_window_days`) | 3 errors from 2 agents within 7 days |
| API error records kept (`error_retention_days`) | 35 days, and never less than the signal window |
| Declined feedback item reopened after, with reports and new agents since the decline (`declined_reopen_after_days`, `declined_reopen_reports`, `declined_reopen_agents`) | 1 day, 5 reports, 3 agents |
| Other open asks on the board a new ask warns of (`duplicate_ask_window_secs`) | younger than 30 minutes |
| Task in `review` with no note from its owner announced after (`review_quiet_secs`) | 1 day, then again each quiet day |
| Task in `doing` whose owner is away announced after (`doing_idle_secs`) | 30 minutes, once per spell away |
| Agent marked `running` but unseen that has left, and is marked `ended` (`owner_left_secs`) | 1 hour |
| Device heartbeat age before `GET /v1/devices` marks it `stale` (`device_stale_secs`) | 90 seconds |
| Batch-coalescing debounce window for `triage` and `verdict` (`judge_batch_debounce_secs`) | 90 seconds (`0` disables) |
| Minimum batch size bypassing triage/verdict debounce (`judge_batch_min_size`) | 3 items (clamped to `>= 1`) |

All defaults are server settings, tuned through the feedback loop (`DESIGN.md`).
