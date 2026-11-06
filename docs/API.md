# HTTP API, version 1

The server's HTTP API is the only boundary: the CLI, the MCP server, the hooks, the device
daemon and the web view all use it. Bodies are JSON; their types are in
`crates/board-proto` (named in brackets below), with `snake_case` fields and enum values and
RFC 3339 UTC timestamps.

## Authentication

| Caller | How | May call |
|---|---|---|
| Device (CLI, MCP server, hooks, daemon) | `Authorization: Bearer <device token>` | everything except "user only" endpoints |
| User in the browser | Identity header set by `tailscale serve` (`Tailscale-User-Login`), matched against the server's `users` setting | everything, including "user only" |
| User on the CLI | A device token created with `--user` | everything, including "user only" |
| Telegram | Not an HTTP caller: the server long-polls the Bot API and accepts answers only from the configured chat id | answering asks |

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
(seen 2m ago); pick another task from ready"`.

## Endpoints

### Health

- `GET /healthz` → `{"ok": true, "seq": <latest event>}`; no authentication.

### Agents

- `POST /v1/agents` `[RegisterAgent]` → `[Agent]`. Idempotent; refreshes repo, branch, cwd,
  title and `last_seen`; a new id with `forked_from` starts with no tasks. An agent named
  as a board's coordinator keeps the `coordinator` role whatever role it registers with.
  Clients register on session start (hooks) and before acting when the agent is unknown.
- `GET /v1/agents?board=&state=` → `[Agent]` list.
- `GET /v1/agents/{id}` → `[Agent]`.
- `POST /v1/agents/{id}/state` `[AgentStateUpdate]` → `[Agent]`. Heartbeat and lifecycle.
  `ended` releases the agent's leases and coordinator lease; owned tasks stay owned.
- `GET /v1/agents/{id}/handoff` → `[Handoff]` or 404.
- `PUT /v1/agents/{id}/handoff` `[PutHandoff]` → `[Handoff]`; 409 on a version mismatch.

### Boards

- `GET /v1/boards` → `[Board]` list. `POST /v1/boards` `[CreateBoard]` → `[Board]`.
- `GET /v1/boards/{slug}` → `[Board]`. `PATCH /v1/boards/{slug}` `[UpdateBoard]` → `[Board]`.
- `POST /v1/boards/{slug}/join` `[JoinBoard]` → `[Board]`.
- `POST /v1/boards/{slug}/coordinator` `[ClaimCoordinator]` → `[Board]`; 409
  `claim_refused` when another device holds the coordinator lease (45 minutes, renewed by
  the daemon and by the coordinator's heartbeats). The device claims first, starts the
  thread, then claims again with the thread's `agent` id, which makes that agent's role
  `coordinator`. `DELETE` releases it; only the holding device or the user may.
- `GET /v1/stats?days=7` → `[Stats]`.

### Tasks

- `GET /v1/tasks?board=&status=&owner=&ready=&mine=&limit=` → `[Task]` list, sorted by
  priority, then id number. `mine=true` uses the `X-Board-Agent` header.
- `POST /v1/tasks` `[CreateTask]` → `[Task]` with status `open`.
- `GET /v1/tasks/{id}` → `[TaskDetail]`.
- `PATCH /v1/tasks/{id}` `[UpdateTask]` → `[Task]`.
- `POST /v1/tasks/{id}/claim` `[TaskAction]` → `[Task]` with status `doing` and the caller
  as owner. Refused with 409 `claim_refused` when a live agent owns it or the caller is at
  its `doing` limit, and with 400 `invalid_transition` while a blocker is not done. A claim
  is one conditional update in one transaction; exactly one of any concurrent claims wins.
- `POST /v1/tasks/{id}/release` `[TaskAction]` → `[Task]` back to `open`, no owner. Owner
  only.
- `POST /v1/tasks/{id}/take` `[TaskAction]` → `[Task]`. Only when `owner_stale`; records a
  `task_taken` event naming the previous owner.
- `POST /v1/tasks/{id}/done` `[DoneTask]` → `[Task]`. Owner only; 400
  `evidence_required` without evidence, or with `check` evidence missing `exit_code`. The
  `task_done` event carries the evidence.
- `POST /v1/tasks/{id}/notes` `[AddNote]` → `[Note]`.

Allowed status changes through `PATCH` (anything else is `invalid_transition`):

| From | To |
|---|---|
| `open` | `blocked`, `dropped` (claiming moves it to `doing`) |
| `doing` | `blocked`, `review`, `open` (same as release), `dropped` |
| `blocked` | `open`, `doing` (owner only), `dropped` |
| `review` | `doing`, `dropped` (`done` only through `/done`) |
| `done`, `dropped` | `open` (reopen; clears the owner) |

`blocked` needs `blocked_reason` or at least one blocker that is not done. Blockers must
be on the same board and must not form a cycle.

### Leases and touches

- `POST /v1/leases` `[CreateLeases]` → `[LeaseResult]`. Advisory: overlapping leases of live
  peers are returned as `conflicts` and raise alerts; nothing is refused.
- `DELETE /v1/leases/{id}` → 204. Owner only.
- `POST /v1/touches` `[Touches]` → `[TouchResult]`: peers that leased or touched any of the
  paths within the last hour.

### Asks (decisions and actions for the user)

- `POST /v1/asks` `[CreateAsk]` → `[Ask]`. Notifies the user (Telegram, if configured).
- `GET /v1/asks?status=open&board=` → `[Ask]` list.
- `GET /v1/asks/{id}` → `[Ask]` in any status (the web view's `/asks/<id>` link).
- `POST /v1/asks/{id}/answer` `[AnswerAsk]` → `[Ask]`. User only; `answered_via` records
  `cli`, `web` or `telegram`. The `ask_answered` event's payload names the ask (`id`) and
  the asking `agent`, and the asker's next alerts include the answer.
- `POST /v1/asks/{id}/withdraw` `[TaskAction]` → `[Ask]`. The asking agent only.

### Snapshots and events

- `GET /v1/snapshot?agent=&kind=full|delta|alerts&since=&format=lines|json|yaml` →
  `[Snapshot]`. Rendered on the server (format in `DESIGN.md`, "Text format"), so every
  client injects the same text. `delta` and `alerts` with nothing relevant return empty
  `text`. The server also advances the agent's cursor to the returned `seq`. `json` is
  compact, so all three formats share the same size caps.
- `GET /v1/events?since=&board=&limit=` → `[EventsPage]`. With
  `Accept: text/event-stream`, streams events as SSE (`id: <seq>`, `data: <Event JSON>`),
  resuming from `Last-Event-ID`, else from `since`.

### Administration (user only)

- `POST /v1/admin/devices` `[CreateDevice]` → `[DeviceToken]` (token shown once).
- `GET /v1/admin/devices` → `[DeviceInfo]` list, never with tokens.
  `DELETE /v1/admin/devices/{name}` revokes; a revoked name may be created again.
- On the server host, `board admin device <name> [--user] --db <file>` creates a device
  without HTTP (the first user token comes from here), and `board admin backup --db <file>
  --out <new file>` writes a consistent copy.

### Web view

Every `GET` outside `/v1` and `/healthz` serves the embedded web view: files under
`/assets/` with immutable caching, any other path without a file extension as
`index.html` (`no-cache`), with a Content-Security-Policy that allows only this origin and
forbids framing.

## Defaults

| Setting | Default |
|---|---|
| Owner counts as stale after | 24 hours unseen |
| File lease and coordinator lease TTL | 45 minutes, renewed by heartbeats |
| Touch overlap window | 1 hour |
| Tasks in `doing` per agent | 1 (claiming a second is refused unless the first is blocked or in review) |
| Done tasks hidden from snapshots after | 30 days |
| Snapshot size caps | full 2,000, delta 600, alerts 400 characters |
| Text limits | title 72, note 280, handoff field 200, question 1,000, body 4,000 characters |

All defaults are server settings, tuned through the feedback loop (`DESIGN.md`).
