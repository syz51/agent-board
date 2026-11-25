# Multi-Machine Scheduler, Unified Runs, and Pluggable Execution Architecture

Status: **Approved target design** (`agent-board-674` Design C+, extended by `agent-board-737`
Agent Board v2 / Design D in Section 8). This document specifies the
no-compromise target architecture for multi-machine work grouping, capability-matched
scheduling, T3-decoupled execution, universal session visibility, and continuous agent-driven
evolution. All server names, tailnet URLs, device names, repositories, and bucket URIs below
are placeholders.

---

## 1. Problem and Architectural Principles

### Why the current board-lease architecture hits a wall

Today `agent-board` dispatches work by leasing an entire **board** to a single device
(`boards.coordinator_device`, 45-minute TTL; see `docs/DESIGN.md`, "Daemon dispatch"). That
model worked well for a single primary laptop, but breaks down on a heterogeneous fleet of
intermittent laptops plus always-on cloud hosts:

1. **Board-level locking prevents cross-machine spillover.** While device A holds a board's
   coordinator lease and hits its local `runs_at_once` cap, an idle device B with 3x the RAM
   cannot execute a worker, review judge, or merge for that board.
2. **Slow, fragile failover for sleeping laptops.** `home_devices` preference order is not
   enforced (`daemon.rs` checks membership only), graceful shutdown does not release leases,
   sleeping laptops leave a board pinned for up to 45 minutes, `watch.json` scheduling state
   is device-local, and review pass verification (`daemon/judges.rs`) trusts only the single
   `dispatch:<device>` identity that started the judge.
3. **Overloaded `Board` abstraction.** One object currently combines four unrelated concerns:
   human project organization, repository binding (picking only the first repo in
   `board.repos`), autopilot/quota policy, and physical machine locking.
4. **Auth-only device model and flat slot accounting.** The `devices` table tracks only bearer
   tokens. The server knows nothing about device online state, OS, RAM/CPU capacity, taints,
   installed runners, or local repositories. Locally, a heavy Rust build and a lightweight
   verdict judge both consume "1 slot."
5. **Tight coupling to T3 Code.** Dispatch assumes local `t3` CLI worktree creation, thread
   listing, and `t3 thread send`, which blocks headless Linux cloud runners, creates blind
   spots for non-T3 sessions, and couples the orchestrator to a single GUI frontend.
6. **External bottlenecks are unmodeled.** Two non-CPU constraints dominate real throughput:
   shared LLM quotas (currently stored as one global Claude-shaped row) and per-repository
   `main` merge serialization (parallel PRs on one repo trigger $O(N^2)$ CI reruns and
   hot-file collisions).

### Why Design C+ (and how it compares to other schedulers)

Evaluated against real-world distributed schedulers and the dual-SQL pull pattern (global SQL
queue + per-node local SQLite):

| Pattern | What we adopt | What we reject and why |
|---|---|---|
| **Global SQL + Local SQLite pull** | Local SQLite WAL per node for write-ahead intent and weighted local resource accounting; task-level pull when a node has headroom. | Replacing `board-server` with an external raw SQL DB (PlanetScale/Postgres): `board-server`'s SQLite WAL already provides atomic `BEGIN IMMEDIATE` arbitration behind `/v1` without bypassing trail-rule invariants, proposal gates, or usage pacing. Postgres remains a clean swap behind the storage trait only if write throughput ever demands it. |
| **Kubernetes `kube-scheduler` + Kueue** | Two-stage **Admission** (global quotas and concurrency limits) before **Filter → Score** placement; **Taints & Tolerations** to isolate control/trust and service hosts from agent builds. | Pure centralized push binding: sleeping laptops invalidate pushed bindings and force retry/re-bind loops that re-invent pull leases with extra moving parts. |
| **Slurm** | **Generic resources / conflict keys**, **Multifactor Priority + Fair-Share** (preventing low-volume deep-work boards from starving behind high-volume triage), and **Backfill** of light jobs into residual capacity. | Static cluster topology and upfront wall-clock runtime estimates (agent turns have unbounded variance). |
| **Buildkite / GitHub Actions** | **Capability-matched pull queues** where awake nodes advertise tags and pull matching work; sleeping nodes simply stop pulling with zero failover delay. | Stateless fire-and-forget job execution (coding agents need sticky multi-turn wake-ups). |
| **Temporal** | **Shared capability queue + Sticky execution** for active sessions with timeout-based fallback to the global pool when a node sleeps or dies. | Full deterministic workflow-history replay engine (Git branches + board notes + session transcripts already provide durable state). |
| **HashiCorp Nomad / Google Omega** | Single transactional state store with optimistic CAS (`version`) and short renewable leases. | Multi-node Raft consensus across intermittent laptops. |

### Core Design Principles

1. **Stable Human Projection, Evolvable Agent Core.** Human interaction lives cleanly at the
   *Projection and Governance* boundary (Boards/Views, Needs-You asks, done summaries, budget
   caps, security-plane taints, and the Trail Rule). Everything below that boundary—scoring
   weights, batching, retry/backfill heuristics, runner adapters, and wire/communication
   protocols—is explicitly versioned, telemetry-backed, and evolvable by agents.
2. **Strict Vendor Terms & Official Unmodified Binaries.** Every provider runner—including
   Antigravity—**must** invoke only the official, unmodified vendor binary through its
   sanctioned public interfaces (such as official CLI or ACP modes). No binary modification,
   no unofficial API spoofing, and no Terms-of-Service workarounds. The `Runner` boundary stays
   strictly provider-agnostic.
3. **Zero Context-Switching Cockpit.** Work executes offline and unattended across the fleet by
   default. Every session—regardless of machine or launcher—is visible in one place, with
   on-demand live tailing, steering, and interactive attach whenever the user chooses to step
   in.
4. **The Trail Rule Holds Everywhere.** Every hold, refusal, lease expiry, orphan transition,
   cancel, protocol fallback, and scheduler skip writes a durable record (time, rule and
   numbers, subject, actor) before taking effect.

---

## 2. Strict 3-Plane Fleet Isolation

Infrastructure stability takes precedence over squeezing build compute out of control-plane
hosts. Every node registers an explicit `role` and default Kubernetes-style **taints** enforced
by the scheduler:

```
┌────────────────────────────────────────────────────────────────────────────┐
│ 1. CONTROL & TRUST PLANE                taint: role=control:NoSchedule     │
│  • control-host (small always-on VPS):  board-server, primary DB, CI gw    │
│  • trust-host   (small always-on VPS):  secrets authority (Vault), metrics │
│  Invariant: ZERO untrusted coding builds; secrets authority never shares   │
│             a kernel with arbitrary agent tool execution.                  │
└────────────────────────────────────────────────────────────────────────────┘
                                   ▲  │  pull / heartbeats / events (tailnet)
                                   │  ▼
┌────────────────────────────────────────────────────────────────────────────┐
│ 2. ELASTIC RUNNER PLANE                 role=runner (untainted / weighted) │
│  • mac-pro (48 GiB, 14c, intermittent): heavy builds, macOS/iOS, bursts    │
│  • mac-air (16 GiB,  8c, intermittent): medium tasks, interactive work     │
│  • runner-linux-1..N (8–32 GiB cloud):  24/7 headless AcpRunner cattle,    │
│    cgroup/container isolated, disposable & horizontally scalable to N      │
└────────────────────────────────────────────────────────────────────────────┘

┌────────────────────────────────────────────────────────────────────────────┐
│ 3. SERVICE HOSTING PLANE                taint: role=service-host:NoSchedule│
│  • service-host-1..N (app VPSs):        deployed production services only  │
│  Invariant: Isolated from both secrets authority and noisy build runners;  │
│             updated via CI/CD deploys, never targeted for agent builds.    │
└────────────────────────────────────────────────────────────────────────────┘
```

- **Taint effects:**
  - `NoSchedule`: the scheduler never leases a run to this node unless the run explicitly
    declares a matching toleration (e.g., an infrastructure-verification command explicitly
    targeting `control-host`).
  - `PreferNoSchedule`: soft penalty in scoring (e.g., tainting `mac-air` with
    `weight=heavy:PreferNoSchedule` so heavy Rust/ML builds prefer `mac-pro` or a large cloud
    runner when available, falling back to `mac-air` only when heavier nodes are offline).
- **Control-plane deterministic loops:** Zero-build control tasks (such as server-side timer
  sweeps, or stateless GitHub PR status/merge automation using scoped tokens) may run safely
  in the control plane or on a tainted `light-only` worker without ever running arbitrary
  repository code or builds.

---

## 3. Decoupled 3-Layer Data and Work Model

To avoid overloading `Board`, execution semantics are separated from human organization into
three explicit layers.

### Layer 1: Execution Primitives (Machine-Optimal)

#### 3.1 `Device` (Fleet Registry)
Extends the auth-only `devices` table with live scheduling metadata split by trust boundary:
- **User/admin-only fields** (`PATCH /v1/admin/devices/{name}`, user token required): `role`
  (`control | runner | service_host`), `taints`, `preference`, and `drain`. A device token can
  never weaken its own plane isolation or clear `NoSchedule` taints.
- **Device-self heartbeat fields** (`PUT /v1/devices/{name}/heartbeat`, strictly authorized to
  that device's own bearer token or a `--user` token; `403 forbidden` for any other device):
  `os`, `arch`, `capacity`, `free_units`, `capabilities`, `runners`, `repos`,
  `protocol_features`, and `heartbeat_at` (sent every 30s while active; stale after
  `device_stale_secs = 90s`).

```json
{
  "name": "mac-pro",
  "role": "runner",
  "status": "online",
  "drain": false,
  "os": "macos",
  "arch": "arm64",
  "capacity": { "weight_units": 12, "mem_gib": 48, "cpu_cores": 14 },
  "free_units": 8,
  "capabilities": ["os:macos", "arch:arm64", "net:global", "tool:xcode", "git:cloneable"],
  "runners": ["acp", "deterministic", "t3", "tmux"],
  "repos": ["github.com/org/agent-board", "github.com/org/dotfiles"],
  "taints": [],
  "preference": 10,
  "protocol_features": ["v1-json", "sse-v1", "run-events-v1"],
  "heartbeat_at": "2026-11-24T08:00:00Z"
}
```

#### 3.2 `Task` Execution Metadata
`Task` remains the durable record of intent, DAG blockers, notes, and acceptance evidence,
gaining optional machine-scheduling fields (falling back to board defaults when unset):

- `repo: Option<String>` — explicit normalized repo key (`host/owner/repo`), fixing multi-repo
  boards where `board.repos[0]` was previously guessed.
- `weight: ResourceWeight` — `light` (1 unit: verdict/triage/merge/docs), `standard` (2 units:
  ordinary coding/review), `heavy` (4 units: Rust workspace builds, ML/data crunching), plus
  optional explicit `resource_units: u32`.
- `requires: Vec<String>` — hard capability predicates (e.g., `["os:macos", "tool:xcode"]`).
  Empty means any untainted runner suffices.
- `tolerations: Vec<TaintToleration>` — explicit opt-in for tainted nodes.
- `conflict_keys: Vec<ConflictKeySpec>` — fleet-wide semaphores acquired atomically when a run
  is leased (defaults automatically derived: `repo:<repo>:write` with `max = board.parallel_tasks`
  for mutating workers; `repo:<repo>:merge` with `max = 1` for merge runs).
- `affinity: Option<Affinity>` — soft locality hint (prefer a node that already has `repo`
  warm, or prefer a specific device when resuming an unpushed branch).
- `preferred_runner: Option<String>` — optional hint (`acp`, `t3`, `deterministic`, etc.).

#### 3.3 `Run` (First-Class Ephemeral Execution Attempt)
Separates *what needs to be done* (`Task`) from *one concrete execution on one machine*
(`Run`). Every managed job and every user-started interactive session is a `Run`:

- **Kinds:** `worker`, `review`, `verdict`, `triage`, `goal`, `curate` *(planned, Section 8)*,
  `merge`, `housekeeping`, `command`, `interactive`.
- **Origins:** `managed` (leased from the global scheduler) or `interactive` (started by the
  user via T3, terminal, IDE, or external UI and reported via hooks).
- **State machine:**
  ```
  (queued) ──▶ leased (3m TTL) ──▶ starting ──▶ running ◄──▶ waiting_external
                                       │          │                 │
                                       ▼          ▼                 ▼
                                    failed    succeeded /       orphaned
                                  / cancelled  preempted       (lease expired)
  ```
  *(Managed work derived dynamically from board/task state enters directly in `leased` on
  claim; explicitly submitted asynchronous jobs such as `command` runs may sit in `queued`
  until claimed.)*
- **Short renewable lease:** Managed runs start with a 3-minute `lease_expires_at`, renewed
  every 30s by the owning daemon. If a laptop closes its lid mid-run, the server sweeper
  transitions the run to `orphaned` after `lease_expires_at` (3m instead of 45m), emits a
  `run_orphaned` event carrying the last known branch/commit/node, releases its `conflict_keys`,
  and makes recovery immediately schedulable on any other awake node.
- **Dispatcher-agnostic rights and review verification:**
  - *Active judge/merger authorization:* Any live run (`state` in `leased | starting | running
    | waiting_external` with unexpired lease) of kind `review | verdict | triage | goal | merge
    | housekeeping` grants its registered `agent_id` coordinator rights on its board—no
    per-board `coordinator` lease required.
  - *Review pass verification:* Merge and completion eligibility checks **both** that the task
    has a standing `review: pass` decision note written after the task's latest move to
    `review` **and** that the note's author matches a registered `review` `Run` for that task
    (never `state = 'succeeded'` alone, since a clean `review: fail` also exits `succeeded`).
    Cross-device handoff between worker, reviewer, and merger is therefore seamless.

#### 3.4 `UsageBucket` (Multi-Provider Quota Accounting)
Replaces the single global Claude-shaped `usage` row with rows keyed by `(provider, account)`
(e.g., `antigravity:default`, `anthropic:default`, `openai:default`). Each bucket independently
tracks its windows, reset timestamps, weekly pace/reserve (preserving the user-only rule for
lowering reserve), and hard stops. Admission checks only the bucket of the runner/model that
would execute an LLM-backed run; deterministic zero-LLM runs (`merge`, `housekeeping`,
deterministic `command`) are exempt. Legacy `PUT/GET /v1/usage` and `PUT /v1/usage/reserve`
map cleanly to the configured default legacy bucket (`anthropic:default`).

### Layer 2: Global Admission and Pull Scheduling (`POST /v1/dispatch/claim`)

When an awake runner daemon has `free_units > 0` (per its local SQLite ledger) and is not
draining, it sends `POST /v1/dispatch/claim` with its `device`, `free_units`, `runners`,
`capabilities`, `repos`, and `supported_kinds`. Inside one `BEGIN IMMEDIATE` SQLite
transaction, `board-server` executes a 5-stage pipeline:

1. **Candidate Enumeration:** Gather all runnable work items across active boards:
   - Ready `auto` tasks (`worker` runs),
   - Tasks in `review` lacking a standing `review: pass` from a registered `review` run
     (`review` runs),
   - Tasks in `review` with a standing `review: pass` (`merge` runs, eligible for
     `DeterministicRunner` — covering open PRs awaiting checks/update/merge, already-merged
     PRs, and no-PR `pr: none` tasks needing `task_done`),
   - Gone-asker answered asks and away-owner releases past grace (`housekeeping` runs,
     eligible for `DeterministicRunner`, ensuring nothing strands when per-board coordinator
     leases are removed),
   - Batched pending proposals on active boards with `verdict_judges = true` (`verdict` runs;
     replaces the racy off-autopilot check in `agent-board-650`),
   - Ready items on the feedback board (`triage` runs),
   - Due goal reviews (`goal` runs), and explicitly queued `command` runs.
2. **Admission Gate (Kueue-style global constraints):** Reject candidates whose:
   - Board is paused/closed or (for non-verdict runs) `autopilot == false`,
   - Target `(provider, account)` `UsageBucket` is over `board.usage_stop_at` or
     `pace_limit == 0` (**LLM-backed runs only**; zero-LLM `merge`/`housekeeping` runs bypass
     LLM quota gates),
   - For **`worker` runs only**: union of tasks in `doing` and active unclaimed `worker` runs
     (`state` in `leased | starting | running`) on that board $\ge \min(\text{parallel\_tasks},
     \text{pace\_limit})$ (preventing double-dispatch before `task_claim` and never starving
     `review`/`merge`/`verdict`/`triage`/`goal` runs),
   - Required `conflict_keys` are at capacity (e.g., `repo:<key>:merge` already held by 1 run,
     eliminating $O(N^2)$ CI thrashing across the entire fleet),
   - For judge runs (`review | verdict | triage | goal`): active judges on that board
     $\ge \text{JUDGES\_AT\_ONCE}$.
3. **Filter (Kubernetes predicates):** Keep only candidates where:
   - Device `role`/`taints` are tolerated by the candidate,
   - Device `capabilities` satisfy every predicate in `requires`,
   - Device `runners` includes a compatible runner (e.g., `merge` needs `deterministic`;
     LLM runs need `acp` or `t3`),
   - `candidate.cost_units <= request.free_units`,
   - If `repo` is set, either `repo` is in `device.repos` or `git:cloneable` is advertised.
4. **Score & Backfill (Slurm multifactor + K8s scoring):** Rank surviving candidates by the
   declarative policy function:
   $$\text{Score} = w_{\text{kind}}\cdot K + w_{\text{prio}}\cdot P + w_{\text{rank}}\cdot R + w_{\text{age}}\cdot A + w_{\text{fair}}\cdot F + w_{\text{loc}}\cdot L + w_{\text{pref}}\cdot D + w_{\text{backfill}}\cdot B$$
   - **Kind priority ($K$):** Finishing in-flight work (`merge`, `review`, `verdict`) outranks
     starting new work (`worker`) so work-in-progress drains cleanly.
   - **Priority & Rank ($P, R$):** Preserves existing `p1 > p2 > p3` and explicit per-priority
     `rank`. *(Note: Section 8.2 supersedes the unbounded linear `w_age` formulation with strict
     non-overlapping priority bands and bounded `ready_at` age so aging never inverts priority or
     explicit rank.)*
   - **Anti-Starvation Age ($A$) & Fair-Share ($F$):** Exponentially decaying board/workstream
     consumption share prevents high-volume boards (e.g., feedback triage) from starving
     long-running engineering boards.
   - **Locality ($L$):** Bonus when the requesting device already has `repo` cloned with warm
     build caches, or when resuming a branch previously touched on that node.
   - **Device Preference ($D$) & Taint Soft Penalty:** Applies `PreferNoSchedule` penalties (e.g.,
     routing heavy builds to `mac-pro` or large Linux runners before `mac-air`).
   - **Backfill ($B$):** If the top global candidate exceeds `request.free_units` (e.g., a 4-unit
     heavy build waiting for a 48 GiB node), smaller nodes seamlessly backfill with 1–2 unit
     jobs without blocking the queue.
5. **Atomic Grant:** Insert the `Run` row in `leased` state (`lease_expires_at = now + 3m`),
   record `conflict_keys` ownership, emit `run_leased` with the winning score breakdown, and
   return the `ClaimedRun` payload. If zero candidates match, record nothing noisy and return
   `204 No Content` with optional hold summary for telemetry.

#### Sticky Execution and Fallback (Temporal Pattern)
- Once a `Run` enters `starting`/`running` on device $X$, `board-server` routes follow-up
  messages for that run/agent (`Answered` ask, `Unblocked` task, `Idle` reminder, human
  `steer`) into $X$'s **sticky wake queue**.
- If $X$ sleeps or disconnects past `sticky_grace_secs` (configurable; default 3m for
  unclaimed leases, 10m for idle/waiting runs unless paused on quota), the server marks the
  run `orphaned`, appends a Trail Rule note naming the branch and last known state, and lets
  any other capable online device pick up continuation from the pushed branch and board notes.

### Layer 3: Human Projection Layer

**Boards, Goals, Needs-You Asks, Done Summaries, and Saved Views** sit strictly *above* Layers
1 and 2:
- A `Board` provides human context (title, goal, default routing policy, `autopilot` switch,
  `usage_stop_at` priority tier, human grouping) and no longer owns a physical device lock.
- Re-organizing tasks across boards or creating cross-board views (by repository, by active
  fleet node, by PR merge queue, by Needs-You asks) never disrupts scheduling or failover.
- Existing `board` CLI, MCP tools, and `/v1` board endpoints remain backward compatible:
  `home_devices` maps smoothly into soft device affinity during transition.

---

## 4. Node Execution Plane, Local SQLite, and T3 Decoupling

### 4.1 Local SQLite WAL (`$BOARD_HOME/state/daemon.db`)

Each node replaces the monolithic JSON `watch.json` with a local SQLite database in WAL mode
(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL` for intent commits). This adopts the
strongest half of the dual-SQL architecture while keeping `board-server` as the single global
arbiter, and directly resolves the write-after-act crash gaps identified in `agent-board-539`:

- **`local_runs`:** Write-ahead intent log persisted *before* creating a git worktree or
  spawning a runner process (`run_id`, `task_id`, `kind`, `runner`, `session_id`, `worktree_path`,
  `branch`, `cost_units`, `pid`, `state`, `lease_expires_at`, `created_at`). On crash/restart,
  the daemon reconciles every non-terminal row against live OS processes / runner status—never
  leaking unnamed branches or double-spawning.
- **`resource_ledger`:** Tracks live weighted units consumed by both `managed` runs and
  locally detected `interactive` sessions (reported by `board hook` through the local
  `hook.sock` relay), computing `free_units = max_units - reserve_units - active_units`.
- **`sticky_wakes` & `unsure_sends`:** Durable wake-up queue with write-before-send digest
  tracking so a hard kill mid-send never drops or duplicates a wake-up.
- **`worktrees`:** Tracks daemon-managed git worktrees, last-used timestamps, and safe cleanup
  eligibility once a PR merges or a run ends cleanly.
- **`event_cursor` & `telemetry_outbox`:** Monotonic SSE cursor plus crash-safe outbox for
  batching run events and completion archive uploads when connectivity flickers.

### 4.2 Daemon-Owned Git Worktrees

Git workspace management moves out of T3 into `board watch`:
- Each device maintains bare/warm repo clones (or configured `[projects]` paths) and creates
  isolated run worktrees under `$BOARD_HOME/worktrees/<repo-slug>/<run-id>` via `git worktree
  add` (with `git fetch origin <base>` and shared build caches where safe, e.g., per-repo
  `CARGO_TARGET_DIR` or sccache on runner VMs).
- Runners receive a plain working directory path (`WorktreeHandle`) and never need to know how
  the worktree was created or cleaned up.

### 4.3 Runtime-Agnostic `Runner` Trait

Replaces the T3-specific `T3` trait (`crates/board/src/t3.rs`) with a clean 6-method execution
boundary (`kind` plus 5 async lifecycle methods):

```rust
#[async_trait]
pub trait Runner: Send + Sync {
    fn kind(&self) -> &'static str; // "acp", "deterministic", "t3", "tmux", ...
    async fn spawn(&self, spec: &RunSpec, workdir: &Path) -> Result<RunHandle>;
    async fn send(&self, handle: &RunHandle, msg: &SteerMessage) -> Result<()>;
    async fn status(&self, handle: &RunHandle) -> Result<RunnerStatus>;
    async fn cancel(&self, handle: &RunHandle, reason: &str) -> Result<()>;
    async fn poll_events(&self, handle: &RunHandle, after_seq: u64) -> Result<Vec<RunEvent>>;
}
```

Concrete implementations:

1. **`AcpRunner` (Primary Default — macOS and Linux):**
   - Invokes the **official, unmodified** vendor binary (e.g., official Antigravity binary in
     sanctioned ACP mode, or any other official ACP-compliant agent binary) over stdio
     JSON-RPC.
   - Persists the official session identifier in `daemon.db` so wake-ups (`send`) and
     steering resume the session cleanly without Electron/GUI overhead.
   - On Linux runner VMs, spawns each run inside a dedicated systemd scope / cgroup slice
     (`MemoryMax`, `CPUQuota`, `TasksMax`) so one runaway build can never destabilize the host.
2. **`DeterministicRunner` (Zero-LLM):**
   - Executes deterministic jobs in native Rust: GitHub PR check polling, `gh pr update-branch`,
     `gh pr merge --match-head-commit`, task completion after merge (or `pr: none`), gone-asker
     / away-owner housekeeping, and structured command runs. Consumes negligible RAM and zero
     LLM tokens.
3. **`T3Runner` (Optional GUI Adapter):**
   - Wraps the official `t3` CLI behind the same `Runner` trait for runs explicitly configured
     to appear as T3 threads, or during migration Slices 1–2.
4. **`TmuxRunner` / Custom Frontend Runners (Extensible):**
   - Any terminal multiplexer, custom UI, or future orchestrator frontend implements the same
     `Runner` trait with zero changes to `board-server`, scheduling policy, or `/v1` contracts.

---

## 5. Universal Session Visibility and Zero Context-Switching Cockpit

Every agent session across every machine—whether launched by `board watch` or started manually
by the user—is visible in one unified `Run` view:

### 5.1 Managed + Interactive Unification
- **Managed runs** (`origin = "managed"`): `board watch` creates the `Run` via
  `/v1/dispatch/claim`, streams compact structured events from `Runner::poll_events`, and
  uploads the full transcript on completion.
- **Interactive / external runs** (`origin = "interactive"`): Whenever the user starts a
  session in T3 Code, a standalone terminal, an IDE, or another UI, existing `board hook`
  events (`SessionStart`, `PostToolUse` 1-min heartbeat, `Stop`, `SessionEnd`) keep working
  strictly within the 1.5-second hook budget with **zero extra HTTP round-trips**:
  - `board-server` upserts and closes the `origin = "interactive"` `Run` **server-side inside
    the existing `POST /v1/agents` and `POST /v1/agents/{id}/state` handlers** (recording
    `device`, `runner`, `agent_id`, `repo`, `branch`, `cwd`, `task_id`, `title`, `attach_hint`,
    and `said` milestones).
  - The local hook relay (`crates/board/src/relay.rs` on `hook.sock`) updates the daemon's
    `daemon.db` `resource_ledger` passively as those existing hook requests pass through (and
    extends its route allowlist for any explicit `/v1/runs` hook routes if added later), so
    autopilot accounts for interactive sessions without scraping T3 CLI lists or risking
    `X-Board-Relay: refused`.

### 5.2 Bounded Live Tail + Object Storage Archival
- **Live event tail (`board-server`):** `POST /v1/runs/{id}/events` accepts compact structured
  events (`seq`, `at`, `kind: phase|tool|note|pr|error|done`, `summary <= 300 chars`, optional
  metadata). The server keeps a bounded ring/table of the **latest 200 events per active run**
  (pruned automatically after retention or archival) and fans them out over SSE
  (`GET /v1/runs/{id}/events`). This gives instant Web UI and `board logs <run> -f` visibility
  from phone or laptop while keeping `control-host`'s disk tiny.
- **Full transcript archive (Object Storage):** On every daemon-observed terminal transition
  (`succeeded`, `failed`, `cancelled`, `preempted`), the owning daemon packages the full
  transcript, prompt, and command outputs and uploads them to configured S3-compatible object
  storage via a configurable archive command (e.g., uploading to
  `s3://archive-bucket/runs/<board>/<run-id>/`), then records `transcript_uri` and
  `transcript_sha256` on the `Run`. If upload fails offline, `daemon.db`'s `telemetry_outbox`
  retries automatically. For `orphaned` runs (marked server-side while the node was offline),
  the original node uploads any partial local transcript best-effort when it reconnects and
  reconciles `daemon.db`; if the node is permanently lost, the run keeps `transcript_uri = null`
  with its server-side Trail Rule orphan note intact.

### 5.3 Single Cockpit Controls (Unattended by Default, Drop-In on Demand)
1. **Hands-off mode:** The Board Web UI Home and Telegram surface only **Needs You (Asks)**,
   **Done Summaries**, and **Fleet Health alerts**.
2. **Peek:** Click any active run (managed or interactive, Mac or cloud) in the Web UI or run
   `board logs <run> -f` to watch structured progress live without SSH or tab-switching.
3. **Steer:** Send a message via Web UI or `board steer <run> "..."` (`POST /v1/runs/{id}/steer`).
   The server enqueues it on the owning device's sticky queue, and the daemon delivers it via
   `Runner::send()`.
4. **Cancel / Preempt:** `board cancel <run>` (`POST /v1/runs/{id}/cancel`) triggers
   `Runner::cancel()` and records a Trail Rule event.
5. **Attach / Takeover:** `board attach <run>` opens the exact local worktree + session
   (terminal/tmux/T3/custom UI) if local, or prints/executes an SSH attach or fresh local
   worktree checkout of `run.worktree_branch` if running on a remote machine.

---

## 6. Built-In Feedback, Telemetry, and Agent-Driven Evolution

This architecture treats **itself** as an evolvable system optimized for agentic work. Human
involvement stays at the projection and governance boundary; agents continuously measure,
critique, and improve the scheduling and communication internals beneath it.

### 6.1 Quantitative Run and Scheduler Telemetry
Every terminal `Run` writes a compact `run_outcomes` record (queryable via
`GET /v1/stats/scheduler?days=N`):
- **Latency:** `ready_to_leased_secs`, `leased_to_running_secs`, `running_secs`,
  `conflict_wait_secs` (time blocked by `conflict_keys`).
- **Efficiency:** `weight_unit_minutes`, `quota_bucket`, `tokens_in`, `tokens_out`,
  `warm_cache_hit: bool`.
- **Quality & Reliability:** `terminal_state`, `orphan_count`, `retry_count`,
  `ci_runs_before_merge`, `review_rounds`, `first_pass_merge: bool`.
- **Fleet Balance:** Per-device utilization, hold causes (`capacity`, `quota`, `conflict_key`,
  `no_matching_device`), and board fair-share skew.

### 6.2 Automated Pathology Signals
Extending `board-server`'s existing `Signals` pipeline (`docs/API.md`, "Signals"), the server
sweeper automatically detects chronic scheduling pathologies and files deduplicated `signal`
items on the feedback board (with exact counts, device IDs, and time windows):
- `signal:scheduler:unschedulable` — ready auto task matches zero online devices for > 1h.
- `signal:scheduler:starvation` — a board's ready task waits > $K$ hours while other boards
  claim continuously.
- `signal:scheduler:orphan_storm` — a device orphans > $N$ runs within a rolling window.
- `signal:scheduler:conflict_hot` — a `conflict_key` blocks claims for > 30% of active hours.
- `signal:runner:spawn_fail` — repeated `Runner::spawn` failures on a runner kind/device.

Workers and judges also continue filing manual setup friction with `about` tags `scheduler`,
`runner`, or `protocol` during their end-of-task review.

### 6.3 Protocol and Capability Negotiation
To allow agents to evolve daemon↔server and agent↔agent communication protocols without
flag-day upgrades:
- Devices declare `protocol_features` (e.g., `["v1-json", "sse-v1", "run-events-v1"]`) on
  registration/heartbeat, and servers advertise supported features on `/healthz` and response
  headers (`X-Board-Features`).
- New encodings or transports (such as compact binary framing, batched event streams, HTTP/2
  multiplexing, WebSockets, or agent-native delta formats) are introduced additively behind
  feature negotiation and benchmarked against baseline token/latency budgets before becoming
  defaults.

### 6.4 Declarative Versioned Policy + Offline Historical Replay
- All 8 scheduling weights ($w_{\text{kind}}, w_{\text{prio}}, w_{\text{rank}}, w_{\text{age}},
  w_{\text{fair}}, w_{\text{loc}}, w_{\text{pref}}, w_{\text{backfill}}$), resource-weight
  mappings, lease TTLs, and default `conflict_keys` live in a versioned declarative
  `scheduler_policy` config (recorded as `policy_version` on every `Run` so before/after
  cohorts are cleanly separable).
- **Deterministic Replay Simulator (`board admin replay-schedule`):** Replays historical
  `events` + `runs` + `run_outcomes` logs under a candidate policy (or shadow-scores live
  claims without mutating state) and outputs comparative metrics (p50/p90 wait, fair-share
  variance, CI reruns/PR, orphan rate).
- **Automated Continuous Evaluation & Guardrailed Self-Tuning (`scheduler-autotune-v1`):**
  - `board-server` runs the exact same deterministic replay engine in-process on a periodic
    sweeper schedule (`[scheduler.autotune]`, default `enabled = true`, `mode = "apply"`,
    `interval_secs = 21600`, `window_days = 7`, `min_runs = 20`, `min_improvement_pct = 5.0`,
    `cooldown_secs = 86400`), recording every evaluation in `scheduler_evaluations`
    (`GET /v1/scheduler/evaluations`, `POST /v1/scheduler/evaluations/run`, surfaced in
    `GET /v1/stats/scheduler`, `board stats --scheduler`, and `/fleet`).
  - **Guardrails:** bounded weight ranges, strict no-regression checks on `tasks_scheduled`,
    `conflict_blocks`, and `p50_wait_secs`, promotion cooldowns, immutable versioned snapshots
    (`v<N>-auto-<YYYYMMDD>`), and an **auto-rollback watchdog** that monitors post-promotion
    `run_outcomes` (`rollback_min_runs = 10`, `rollback_orphan_delta = 0.05`,
    `rollback_p90_ratio = 1.25`) and automatically reactivates `previous_version` with a Trail
    Rule event and diagnostic feedback signal if live performance degrades.
- **Evolution Rule:** Any agent proposing a manual scheduling heuristic or protocol change
  backs its `Proposal:` with replay/shadow evidence, passes independent verdict + code review,
  and declares a 14-day `Revisit:` task (`docs/DESIGN.md`, "Revisits") with concrete
  pass/rollback bars.

### 6.5 Governance Boundary (Autonomous vs. Human-Approved)
- **Agents evolve freely** (through Proposal → Verdict Judge → Replay/Test Evidence → Reviewed
  PR): scoring weights, fair-share/backfill tuning, local SQLite indexes, runner adapters,
  telemetry signals, wire-protocol extensions, and worktree/cache optimizations.
- **Human `ask_user` required** (per ask-policy reasons `design`, `cost`, `security`,
  `irreversible`): changes to the Human Projection UX, provider spending/quota ceilings,
  3-plane security taints/isolation boundaries, vendor ToS / official-binary rules, or
  irreversible retention policies.

---

## 7. Backward-Compatible `/v1` Contract and 4-Slice Rollout Plan

All changes stay strictly additive within `/v1` (`crates/board-proto`, `docs/API.md`,
`docs/DESIGN.md`, and tests updated together per slice) so existing daemons and MCP clients
keep operating throughout the rollout:
- Every new optional field on existing wire types (`Task`, `Board`, `DeviceInfo`, etc.) carries
  `#[serde(default)]` and `skip_serializing_if` so newer clients read older servers and older
  clients ignore new fields cleanly.
- Every newly introduced extensible enum (`RunKind`, `RunState`, `ResourceWeight`, `DeviceRole`,
  `RunEventKind`, `RunOrigin`) is declared as an **open enum** with `#[serde(other)] Unknown`
  (and `oneOfOrUnknown` in `web/`), matching `docs/API.md`, "Compatibility," so future
  agent-driven evolution can add new kinds without breaking running clients.

### Additive Schema Migrations (`0031+`)
- `0031_device_capabilities_and_usage.sql` *(implemented in Slice 1.1)*: extends `devices` with
  `role`, `os`, `arch`, `capacity_json`, `free_units`, `capabilities_json`, `runners_json`,
  `repos_json`, `taints_json`, `preference`, `protocol_features_json`, `drain`, `heartbeat_at`;
  adds optional `repo`, `weight`, `resource_units`, `requires_json`, `tolerations_json`,
  `conflict_keys_json`, `affinity_json`, `preferred_runner` to `tasks`; adds `usage_buckets`
  keyed by `(provider, account)` and `usage_bucket_samples` (backfilling any existing `usage`,
  `usage_pace`, and `usage_weekly` rows into `('anthropic', 'default')` while aliasing legacy
  `/v1/usage` and `/v1/usage/reserve` to `('anthropic', 'default')`).
- `0032_runs_and_coordination.sql` *(implemented in Slice 1.2)*: adds `runs`, `run_events`
  (bounded tail), `run_outcomes`, `conflict_locks`, and `sticky_wakes` tables, `/v1/runs`
  endpoints, `runs-v1` feature flag, lease-expiry orphan sweeper, and dispatcher-agnostic
  judge/merger rights & standing `review: pass` verification.
- `0033_scheduler_policy.sql` *(planned)*: adds versioned `scheduler_policy` snapshots and
  pathology signal tracking.

### Planned Additive `/v1` Endpoints
- `PATCH /v1/admin/devices/{name}` — **user-only** update of `role`, `taints`, `preference`,
  and `drain` (protects 3-plane isolation from device self-escalation).
- `PUT /v1/devices/{name}/heartbeat` — **device-self-only** (or user token) update of live
  capabilities, free weighted units, repos, runners, and protocol features.
- `GET /v1/devices` — fleet inventory with live health, capacity, roles, taints, and active
  run counts.
- `POST /v1/dispatch/claim` — atomic Admission → Filter → Score → Lease grant.
- `GET /v1/runs`, `GET /v1/runs/{id}`, `POST /v1/runs` (explicit run submit/upsert),
  `PATCH /v1/runs/{id}` (heartbeat/state/outcome/transcript URI).
- `POST /v1/runs/{id}/events` & `GET /v1/runs/{id}/events` (SSE bounded live tail).
- `POST /v1/runs/{id}/steer` & `POST /v1/runs/{id}/cancel`.
- `GET /v1/stats/scheduler` — quantitative scheduler/run telemetry.
- `PUT /v1/usage/{provider}/{account}`, `PUT /v1/usage/{provider}/{account}/reserve`
  (user-only to lower/disable, matching `/v1/usage/reserve`), and `GET /v1/usage/buckets`.

### 4 Vertical Delivery Slices (Zero Throwaway Work)

1. **Slice 1 — Core Primitives & Local SQLite Foundation:**
   - Server: `0031`–`0033` migrations (`devices` registry with admin-only role/taints, `runs` +
     `conflict_locks`, `usage_buckets`, task routing fields), dispatcher-agnostic judge/merge
     rights and standing `review: pass` verification.
   - Daemon: replace `watch.json` with local SQLite WAL (`daemon.db`) with write-before-act
     intent (incorporating `agent-board-539`), weighted local resource ledger, and device-self
     heartbeats.
2. **Slice 2 — Capability-Matched Pull Scheduler & Multi-Mac Fleet Live:**
   - Server: `POST /v1/dispatch/claim` (Admission → Filter → Score + per-repo merge
     serialization via `conflict_keys` + fair-share + backfill + `housekeeping` recovery), 3m
     run lease sweeper + sticky wake fallback, and unified proposal verdict claiming (resolving
     `agent-board-650`).
   - Daemon: pull-based dispatch loop + `DeterministicRunner` for PR merges and housekeeping;
     bring secondary Mac online in the shared pull pool alongside primary Mac.
3. **Slice 3 — T3 Decoupling, Official-Binary `AcpRunner` & Cloud Runners:**
   - Daemon: move git worktree lifecycle into `board watch`, extract the 6-method `Runner`
     trait, implement `AcpRunner` (strictly invoking official unmodified vendor binaries via
     sanctioned ACP/CLI interfaces) alongside `T3Runner`, and add Linux cgroup/scope isolation.
   - Infra: onboard the first dedicated Linux cloud runner VM into the Runner Plane while
     keeping Control & Trust Plane hosts tainted `role=control:NoSchedule`.
4. **Slice 4 — Universal Visibility Cockpit & Self-Evolution Loop:**
   - Hooks + Daemon: server-side interactive run upsert inside existing `/v1/agents` +
     `/v1/agents/{id}/state` hook calls (preserving the 1.5s hook budget) with passive
     `hook.sock` ledger accounting, live bounded event tailing (`POST/GET /v1/runs/{id}/events`),
     and completion transcript archival to configured S3-compatible object storage.
   - Web UI + CLI: unified Runs & Fleet cockpit, `board logs -f`, `board steer`, `board attach`,
     and `board cancel`.
   - Evolution loop: `run_outcomes` + `GET /v1/stats/scheduler`, automated pathology signals to
     the feedback board, versioned `scheduler_policy`, and `board admin replay-schedule`
     offline simulator (extending `agent-board-135` into an ongoing self-review cadence).

---

## 8. Agent Board v2 — The Lean Autonomous Factory (Design D + Zero-Overhead Runtime)

Production profiling across the live multi-Mac fleet revealed three structural pathologies in the
initial Design C+ rollout: (1) unstable backlog growth ($\lambda \approx 1.8\mu$) with ~100%
agent-generated proposals and unused `rank` infrastructure, (2) `w_age` inverting `P1 > P2 > P3`
after 7 days and beating `rank = 1` after 21 hours while `autotune` chased global `p90_wait`, and
(3) 6–9 separate LLM cold starts per landed feedback PR (double-hop `triage -> verdict`, redundant
`diff-review` + `review` judge, 20+/day empty-inbox `board-feedback` `goal` runs, LLM `AfterMerge`
for mechanical `deploy/upgrade.sh`, and volatile `SessionStart`/`PreInvocation` hooks busting
cross-run KV prefix caching).

Agent Board v2 unifies backlog prioritization (Design D) and token-lean execution into one
closed-loop architecture built on five engineering principles (Toyota TPS pull/overproduction
elimination, compiler predicate pushdown + lazy evaluation + tiered JIT, KV-cache prefix
stability, CoDel AQM + WSJF + RTOS priority inheritance, and single authoritative verification):

### 8.1 Zero-Overhead, Prefix-Stable Runtime (Epic 1: `agent-board-739`, planned)
- **Zero board hooks inside managed runs (`AGENT_BOARD_HOOKS=off`):** Managed workers and judges
  receive exact pre-packed state from `prepack.rs`; hooks (`SessionStart`, `PreInvocation`,
  `PostToolUse`, `Stop`) are suppressed so they neither add latency nor bust prompt caching.
- **Interactive hook hygiene:** `PreInvocation` never dumps cached full snapshots on timeout or
  recovery (`offline_notified` ping-pong removed) and skips bare count-only deltas; subagents
  never absorb parent `SessionStart` snapshots.
- **Static-before-Dynamic KV prefix layout:** Static instructions come first (keeping always-loaded
  global agent instructions lean with on-demand reference docs, and omitting conditional
  `Climbs`/`Revisits` sections when irrelevant), followed by per-task pre-packed context at the
  tail (`Worker`, `Verdict` with `board.goal`, `Review` with `gh pr view` + `diff --stat` +
  bounded diff, and `Curate`/`Goal`).
- **One authoritative review per PR:** Daemon-dispatched workers are exempt from spawning an
  internal `diff-review` subagent; the managed `Review` judge is the single authoritative
  independent review. Respecting Section 2's 3-plane isolation, `board-server` gates `Review`
  dispatch strictly on board state (PR URL or `pr: none` note present, and owner not running)
  without calling GitHub, while the runner daemon checks CI status via `gh` and pre-packs the
  diff + CI check summary into the review brief. Interactive/human sessions keep the single
  `diff-review` subagent rule.

### 8.2 Strict Economic Dispatch & `/v1`-Safe v2 Schema (Epic 2: `agent-board-740`, planned)
- **Migration `0037_backlog_v2.sql` (strictly additive in `/v1`):**
  - `tasks`: `origin` (`user` | `feedback` | `agent`), `size` (`xs` | `s` | `m` | `l`),
    `cooled_at` (nullable timestamp — `/v1`-safe Icebox representation on `status = 'open'` so
    `TaskStatus` never breaks older clients), `ready_at` (when a task actually became ready), plus
    server-computed `report_count` and blocker Priority Inheritance `effective_priority`.
  - `boards`: `kind` (`project` | `inbox`; feedback inbox boards backfilled to `inbox` with
    `goal_review_every_secs = 0`) and `curate_every_secs`.
- **Strict priority envelopes, per-priority `rank`, & board-wide Up Next Frontier:**
  - `rank` (`1..N`) remains **per priority level** (matching `board rank` and `docs/DESIGN.md`).
  - A board's **Up Next Frontier** is defined unambiguously as the **top 20 ready tasks across the
    board** in strict dispatch order (`p1` ranked → `p1` unranked → `p2` ranked → `p2` unranked →
    `p3` ranked).
  - `Worker` dispatch uses non-overlapping priority bands (`P1 > P2 > P3` via `effective_priority`),
    explicit `rank` primary inside each band (`[50, 200]`), deterministic unranked WSJF tie-break
    `wsjf = (1.0 + 0.5 * min(report_count, 10) + 0.5 * min(unblocks_count, 6)) / size_weight`
    (with `size_weight = { xs: 1.0, s: 1.5, m: 3.0, l: 6.0, None: 2.0 }`, scaled into `[0, 40]` —
    no separate `impact` column), and `ready_at` age strictly capped at `<= 8.0` (never inverting
    priority or explicit rank). `origin = user` tasks auto-place at `rank = 1` of their level
    (`agent-board-615`).
- **Intake & signal hygiene:** `Verdict`/`Triage` batches sort by `(effective_priority,
  report_count DESC, rank IS NULL, rank, created_at)` (`agent-board-602`); `Goal` never runs on
  `inbox` boards; `Review` is server-gated on PR existence (or `pr: none`) and non-running owner
  (eliminating 23% cancellations); workers whose declared paths overlap active `doing` leases are
  skipped (`agent-board-597`); deferred `Merge` polls back off exponentially (`60s -> 180s ->
  300s`) without writing empty `run_outcomes`; `pathology.rs` respects quota holds and counts LLM
  runs only; `autotune.rs` enforces strict priority/rank bounds (`w_age <= 8.0`) and excludes
  deterministic/cancelled polls.

### 8.3 Closed-Loop `Curate`, 1-Hop Triage, 30d Icebox & Deterministic Deploy (Epic 3: `agent-board-741`, implemented)
- **0-LLM feedback dedupe & auto-revive (`crates/board-server/src/ops/feedback.rs`):** Before
  leaving a new feedback item or `signal:*` report open for `triage`, the server checks open and
  cooled Icebox tasks on the board and routed target boards for an exact `signal:*` key match or
  strong same-`about` normalized title match (exact normalized title with `>= 2` tokens or
  Jaccard `>= 0.70` with `>= 4` significant tokens). On match, it attaches the report to the
  existing task (updating `report_count`), auto-revives cooled Icebox tasks (`cooled_at = NULL`,
  `ready_at = now` with a Trail Rule `task_updated` event), and auto-resolves the duplicate item
  without waking an LLM `triage` judge.
- **1-hop `Triage` (`prompts/judges/triage.md`, compatible with `is_pending_proposal`):** Routine
  `xs`/`s`/`m` feedback routes directly to target boards with `--origin feedback --size <xs|s|m>
  --auto true` and a non-`Proposal:` body prefix (`Feedback <id>: ...`) in a single LLM pass;
  `triage` uses the `Proposal:` prefix (`--auto false`, gated by `is_pending_proposal`) only for
  `size l`, security, irreversible, or cross-repo changes.
- **Lazy admission + daily `Curate` (`RunKind::Curate`, `prompts/judges/curate.md`):** Non-fast-lane
  `p2`/`p3` agent `Proposal:` tasks on active autopilot `project` boards (with `curate_every_secs
  != Some(0)`) wait in `Proposals` at 0 token cost until the board's daily pre-packed `curate`
  pass (default `CURATE_EVERY_SECS_DEFAULT = 86_400`, or frontier-refill when the ranked Up Next
  Frontier drops below 10 tasks with at least 2 ready tasks and a 1h cooldown). `Curate` runs 1
  at a time per board (`kind_factor = 0.35`, `cost_units = 2`, coordinator rights via
  `LIVE_RUN_RIGHTS`, configurable via `[judge_models.curate]`, pre-packed with goal + 14d `λ/μ`
  ratio, Top-15 Frontier, up to 15 pending Proposals, up to 20 unranked WSJF candidates, and
  `git log -n 30`). `P1`, `origin = user`, `report_count >= 3`, and boards with `curate_every_secs
  == Some(0)` or off autopilot stay on the immediate `Verdict` fast-lane.
- **Deterministic 30d CoDel Icebox & Deploy (`ops/tasks.rs::sweep_icebox`, `deterministic_runner.rs`):**
  The minute sweeper auto-cools **only** unranked (`rank IS NULL`), `status = 'open'`,
  `priority = 'p3'`, `origin = 'agent'` tasks unstarted and untouched (no notes or updates) for
  30 days with no active run (`cooled_at = now`, Trail Rule `task_updated` event with
  `reason = "ttl_cooled"`). Explicitly exempt from cooling: `p1`, `p2`, `origin = 'user'`,
  `origin = 'feedback'`, any ranked task (`rank IS NOT NULL`), tasks blocking open/active work,
  and tasks with future `not_before`. Standard mechanical `deploy/upgrade.sh --yes <sha>`
  after-merge checks route to `DeterministicRunner` (`RunKind::Merge`, 0 LLM tokens), which waits
  for `main` CI `image` job success, runs `deploy/upgrade.sh --yes <sha>`, records `review: pass`
  + check evidence, and closes the task; any custom check, CI failure, timeout, or non-zero exit
  writes a Trail Rule `finding: deterministic after-merge deploy failed` note and falls back
  cleanly to the LLM `AfterMerge` review judge.

### 8.4 Operator-First Web UI (Epic 4: `agent-board-742`, planned)
- Four clear tabs per board (**Up Next (Top-20 Frontier) · Proposals · Backlog · Icebox**) aligned
  with exact dispatch order, displaying per-level rank badges, `size`, `report_count`, `User`
  origin, and 1-click **Pin to Top**, **Drag-to-Rank**, **Priority**, and **Icebox/Drop** controls.
