// Wire types of `crates/board-proto` (model.rs, api.rs), as decoders. The TypeScript types
// are derived from the decoders so the two cannot drift apart.

import {
  array,
  boolean,
  type Decoded,
  integer,
  json,
  nullable,
  number,
  object,
  oneOf,
  oneOfOrUnknown,
  record,
  string,
  timestamp,
  withDefault,
} from "./decode.ts";

export const TASK_STATUSES = ["open", "doing", "blocked", "review", "done", "dropped"] as const;
export const PRIORITIES = ["p1", "p2", "p3"] as const;
export const BOARD_KINDS = ["project", "inbox"] as const;
export const TASK_ORIGINS = ["user", "feedback", "agent"] as const;
export const TASK_SIZES = ["xs", "s", "m", "l"] as const;
export const DISPATCH_HOLD_REASONS = [
  "drain",
  "quota",
  "conflict_key",
  "capacity",
  "taint",
  "requires",
  "parallel_tasks",
  "judge_cap",
  "runner",
  "repo",
  "lease_overlap",
] as const;

const boardStatus = oneOf("active", "paused", "closed");
const boardKind = oneOfOrUnknown(...BOARD_KINDS);
const taskStatus = oneOf(...TASK_STATUSES);
const priority = oneOf(...PRIORITIES);
const taskOrigin = oneOfOrUnknown(...TASK_ORIGINS);
const taskSize = oneOfOrUnknown(...TASK_SIZES);
export const dispatchHoldReason = oneOfOrUnknown(...DISPATCH_HOLD_REASONS);

export const statusCounts = object({
  open: integer,
  doing: integer,
  blocked: integer,
  review: integer,
  done: integer,
  dropped: integer,
  ready: integer,
  /** The ready tasks with `auto` on; `null` from a server that predates it. */
  ready_auto: nullable(integer),
});

export const board = object({
  slug: string,
  title: string,
  goal: string,
  status: boardStatus,
  temporary: boolean,
  repos: array(string),
  autopilot: boolean,
  kind: nullable(boardKind),
  goal_review_every_secs: nullable(integer),
  curate_every_secs: nullable(integer),
  coordinator: nullable(string),
  home_devices: array(string),
  counts: statusCounts,
  open_asks: integer,
  created_at: timestamp,
  updated_at: timestamp,
});

export const RESOLUTION_KINDS = ["routed", "duplicate", "forwarded", "declined", "fixed"] as const;

/** How a feedback item was resolved; `task` is set for routed and duplicate. */
const feedbackResolution = object({
  kind: oneOfOrUnknown(...RESOLUTION_KINDS),
  task: nullable(string),
  reason: nullable(string),
  at: timestamp,
  by: string,
});

/** A feedback item's report: set only on tasks filed through `POST /v1/feedback`. */
const feedback = object({
  about: string,
  activity: nullable(string),
  source: oneOf("agent", "capture", "signal"),
  signature: string,
  reports: integer,
  reporters: array(string),
  last_reported_at: timestamp,
  resolution: nullable(feedbackResolution),
  /** An earlier item for the same report, resolved and fixed, that came back. */
  recurs: nullable(string),
});

export const task = object({
  id: string,
  board: string,
  parent: nullable(string),
  title: string,
  body: string,
  acceptance: string,
  status: taskStatus,
  priority,
  /** Blocker Priority Inheritance effective priority; absent or null on older servers or when equal. */
  effective_priority: nullable(priority),
  /** The task's place inside its priority level, lower first; none follows the ranked. */
  rank: nullable(integer),
  /** Where the task came from (`user`, `feedback`, `agent`); null from older servers. */
  origin: nullable(taskOrigin),
  /** T-shirt effort size (`xs`, `s`, `m`, `l`); null when unset or from older servers. */
  size: nullable(taskSize),
  /** Non-null when an `open` task has been cooled into the Icebox. */
  cooled_at: nullable(timestamp),
  /** When the task most recently became ready; null when unset or from older servers. */
  ready_at: nullable(timestamp),
  /** Deduplicated report count across signals/feedback; 0 from older servers. */
  report_count: withDefault(integer, 0),
  blocked_reason: nullable(string),
  owner: nullable(string),
  owner_stale: boolean,
  blockers: array(string),
  ready: boolean,
  /**
   * Blockers hold the task though every one is closed: it is `blocked`, or `open` with a
   * dropped blocker, which never counts as done. The board moves nothing; false from older
   * servers.
   */
  blockers_closed: withDefault(boolean, false),
  /**
   * A pending proposal: open, not `auto`, with a body starting `Proposal:`. Agents review it
   * with a `verdict:` note; it is never ready work.
   */
  proposal: withDefault(boolean, false),
  /** Not ready before this time; the server clears it once passed. */
  not_before: nullable(timestamp),
  auto: boolean,
  created_by: string,
  version: integer,
  created_at: timestamp,
  updated_at: timestamp,
  feedback: nullable(feedback),
  /** What the agent that finished the task told the user; untrusted. Absent unless done with one. */
  summary: nullable(string),
  /** How many tasks name this one as parent, whatever their status; 0 from older servers. */
  child_count: withDefault(integer, 0),
  /** The owner's short handle, such as `t3:7f3a`. Absent without an owner and from older servers. */
  owner_handle: nullable(string),
  /** In a search result: text around the words found, marked with `[` and `]`; untrusted. */
  snippet: nullable(string),
  /** In a search result: no task holds every word, and this one holds at least one. */
  any_word: withDefault(boolean, false),
});

/** A child task as its parent's detail lists it. */
export const childTask = object({
  id: string,
  title: string,
  status: taskStatus,
  ready: boolean,
  owner: nullable(string),
  owner_stale: boolean,
});

/**
 * Event types are kept as plain strings: the server may add types within `/v1`, and the
 * web view shows unknown ones generically instead of failing.
 */
export const event = object({
  seq: integer,
  at: timestamp,
  board: nullable(string),
  actor: string,
  type: string,
  task: nullable(string),
  payload: json,
});

export const note = object({
  id: integer,
  task: string,
  agent: string,
  kind: oneOf("note", "finding", "decision", "feedback"),
  text: string,
  at: timestamp,
});

export const evidence = object({
  kind: oneOf("check", "commit", "pr", "artifact"),
  value: string,
  exit_code: nullable(integer),
  agent: string,
  at: timestamp,
});

export const taskDetail = object({
  task,
  notes: array(note),
  evidence: array(evidence),
  blocks: array(string),
  children: array(string),
  /** The children with their titles and statuses; empty from servers that predate it. */
  child_tasks: withDefault(array(childTask), []),
  history: array(event),
});

export const agent = object({
  id: string,
  handle: string,
  runtime: oneOf("claude", "codex", "opencode", "antigravity"),
  device: string,
  role: oneOf("worker", "coordinator"),
  title: nullable(string),
  repo: nullable(string),
  branch: nullable(string),
  cwd: nullable(string),
  native_ids: record(string),
  forked_from: nullable(string),
  state: oneOf("running", "idle", "paused", "ended"),
  paused_until: nullable(timestamp),
  boards: array(string),
  last_seen: timestamp,
  created_at: timestamp,
});

export const handoff = object({
  agent: string,
  now: string,
  next: string,
  blockers: string,
  version: integer,
  updated_at: timestamp,
});

export const ask = object({
  id: integer,
  board: string,
  task: nullable(string),
  agent: string,
  kind: oneOf("decision", "action"),
  question: string,
  options: array(string),
  status: oneOf("open", "answered", "withdrawn"),
  answer: nullable(string),
  answered_via: nullable(string),
  created_at: timestamp,
  answered_at: nullable(timestamp),
});

/** The last message of one of a coordinator's turns, kept for the user; `text` is untrusted. */
export const said = object({
  id: integer,
  agent: string,
  board: string,
  text: string,
  at: timestamp,
});

export const eventsPage = object({ events: array(event), next_since: integer });

export const health = object({ ok: boolean, seq: integer });

/** `ErrorCode` is kept open for the same reason as event types. */
export const apiError = object({ code: string, message: string, next: nullable(string) });

export const rankedTasks = object({
  tasks: array(task),
});

export const RUN_KINDS = [
  "worker",
  "review",
  "verdict",
  "triage",
  "goal",
  "curate",
  "merge",
  "housekeeping",
  "command",
  "interactive",
] as const;

export const RUN_ORIGINS = ["managed", "interactive"] as const;

export const RUN_STATES = [
  "queued",
  "leased",
  "starting",
  "running",
  "waiting_external",
  "succeeded",
  "failed",
  "preempted",
  "orphaned",
  "cancelled",
] as const;

export const RUN_EVENT_KINDS = [
  "phase",
  "tool",
  "note",
  "message",
  "pr",
  "error",
  "cancelled",
  "done",
] as const;

export const DEVICE_ROLES = ["control", "runner", "service_host"] as const;
export const DEVICE_STATUSES = ["online", "stale", "offline"] as const;
export const TAINT_EFFECTS = ["no_schedule", "prefer_no_schedule"] as const;
export const AUTOTUNE_MODES = ["off", "propose", "apply"] as const;
export const AUTOTUNE_DECISIONS = [
  "keep",
  "promoted",
  "proposed",
  "rolled_back",
  "skipped_insufficient_data",
] as const;

export const conflictLock = object({
  key: string,
  run_id: string,
  max_holders: integer,
  acquired_at: timestamp,
});

export const run = object({
  id: string,
  task_id: nullable(string),
  board: nullable(string),
  kind: oneOfOrUnknown(...RUN_KINDS),
  origin: withDefault(oneOfOrUnknown(...RUN_ORIGINS), "managed"),
  device: string,
  runner: string,
  agent_id: nullable(string),
  state: oneOfOrUnknown(...RUN_STATES),
  lease_expires_at: nullable(timestamp),
  worktree_branch: nullable(string),
  cwd: nullable(string),
  title: nullable(string),
  attach_hint: nullable(string),
  cost_units: withDefault(integer, 1),
  policy_version: nullable(string),
  outcome: nullable(json),
  transcript_uri: nullable(string),
  transcript_sha256: nullable(string),
  conflict_locks: withDefault(array(conflictLock), []),
  created_at: timestamp,
  started_at: nullable(timestamp),
  ended_at: nullable(timestamp),
  updated_at: timestamp,
});

export const runEvent = object({
  run_id: string,
  seq: integer,
  at: timestamp,
  kind: oneOfOrUnknown(...RUN_EVENT_KINDS),
  summary: string,
  meta: nullable(json),
});

export const steerRunResponse = object({
  wake_id: integer,
  run_id: string,
  device: string,
});

export const cancelRunResponse = object({
  run_id: string,
  device: string,
  state: oneOfOrUnknown(...RUN_STATES),
  wake_id: nullable(integer),
});

export const taint = object({
  key: string,
  value: nullable(string),
  effect: oneOfOrUnknown(...TAINT_EFFECTS),
});

export const deviceCapacity = object({
  weight_units: integer,
  mem_gib: nullable(integer),
  cpu_cores: nullable(integer),
});

export const deviceEntry = object({
  name: string,
  role: withDefault(oneOfOrUnknown(...DEVICE_ROLES), "runner"),
  status: oneOfOrUnknown(...DEVICE_STATUSES),
  drain: withDefault(boolean, false),
  os: nullable(string),
  arch: nullable(string),
  capacity: nullable(deviceCapacity),
  free_units: nullable(integer),
  capabilities: withDefault(array(string), []),
  runners: withDefault(array(string), []),
  repos: withDefault(array(string), []),
  taints: withDefault(array(taint), []),
  preference: withDefault(integer, 0),
  protocol_features: withDefault(array(string), []),
  heartbeat_at: nullable(timestamp),
});

export const usageWindow = object({
  percent: number,
  resets_at: nullable(timestamp),
});

export const usagePace = object({
  reserve: number,
  day_start: timestamp,
  anchor: number,
  line: number,
  projected: number,
  hard_since: nullable(timestamp),
});

export const usageBucket = object({
  provider: string,
  account: string,
  five_hour: nullable(usageWindow),
  seven_day: nullable(usageWindow),
  read_at: nullable(timestamp),
  device: nullable(string),
  reserve: nullable(number),
  pace: nullable(usagePace),
});

export const schedulerWeights = object({
  w_kind: withDefault(number, 100),
  w_prio: withDefault(number, 30),
  w_rank: withDefault(number, 10),
  w_age: withDefault(number, 16),
  w_fair: withDefault(number, 12),
  w_loc: withDefault(number, 8),
  w_pref: withDefault(number, 1),
  w_backfill: withDefault(number, 6),
});

export const schedulerPolicy = object({
  version: string,
  weights: schedulerWeights,
  lease_ttl_secs: integer,
  sticky_grace_secs: integer,
  notes: nullable(string),
  active: boolean,
  created_by: nullable(string),
  created_at: timestamp,
});

export const schedulerPolicyResponse = object({
  version: string,
  weights: schedulerWeights,
  lease_ttl_secs: integer,
  sticky_grace_secs: integer,
  notes: nullable(string),
  active: boolean,
  created_by: nullable(string),
  created_at: timestamp,
  history: withDefault(array(schedulerPolicy), []),
});

export const latencyPercentiles = object({
  count: integer,
  p50: nullable(number),
  p90: nullable(number),
});

export const deviceSchedulerStats = object({
  device: string,
  status: oneOfOrUnknown(...DEVICE_STATUSES),
  runs: integer,
  succeeded: integer,
  failed: integer,
  orphaned: integer,
  weight_unit_minutes: number,
});

export const boardSchedulerStats = object({
  board: string,
  runs: integer,
  succeeded: integer,
  orphaned: integer,
  ready_to_leased_p50_secs: nullable(number),
  ready_to_leased_p90_secs: nullable(number),
  weight_unit_minutes: number,
  fair_share_percent: number,
  holds_by_reason: withDefault(record(integer), {}),
});

export const policyVersionStats = object({
  policy_version: string,
  runs: integer,
  succeeded: integer,
  failed: integer,
  orphaned: integer,
  orphan_rate: number,
  ready_to_leased_p50_secs: nullable(number),
  ready_to_leased_p90_secs: nullable(number),
  running_p50_secs: nullable(number),
  first_pass_merge_rate: nullable(number),
  warm_cache_hit_rate: nullable(number),
  total_weight_unit_minutes: number,
});

export const replayMetrics = object({
  policy_version: string,
  tasks_scheduled: integer,
  p50_wait_secs: nullable(number),
  p90_wait_secs: nullable(number),
  fair_share_gini: number,
  conflict_blocks: integer,
  backfill_count: integer,
  locality_hits: integer,
});

export const schedulerEvaluation = object({
  id: integer,
  evaluated_at: timestamp,
  mode: oneOfOrUnknown(...AUTOTUNE_MODES),
  decision: oneOfOrUnknown(...AUTOTUNE_DECISIONS),
  baseline_policy_version: string,
  candidate_policy_version: nullable(string),
  applied_policy_version: nullable(string),
  previous_policy_version: nullable(string),
  window_days: integer,
  sample_runs: integer,
  sample_tasks: integer,
  baseline_metrics: nullable(replayMetrics),
  candidate_metrics: nullable(replayMetrics),
  candidate_weights: nullable(schedulerWeights),
  score_delta_pct: nullable(number),
  reason: string,
  feedback_task_id: nullable(string),
});

export const schedulerEvaluationsResponse = object({
  mode: oneOfOrUnknown(...AUTOTUNE_MODES),
  enabled: boolean,
  interval_secs: integer,
  evaluations: withDefault(array(schedulerEvaluation), []),
});

export const kindTokenStats = object({
  runs: integer,
  tokens_in: integer,
  cached_tokens: integer,
  tokens_out: integer,
  reasoning_tokens: integer,
  cache_hit_rate: nullable(number),
});

export const schedulerStats = object({
  days: integer,
  since: timestamp,
  board: nullable(string),
  policy_version: nullable(string),
  runs_total: integer,
  runs_by_kind: withDefault(record(integer), {}),
  runs_by_terminal_state: withDefault(record(integer), {}),
  orphan_rate: number,
  first_pass_merge_rate: nullable(number),
  warm_cache_hit_rate: nullable(number),
  ready_to_leased_secs: latencyPercentiles,
  leased_to_running_secs: latencyPercentiles,
  running_secs: latencyPercentiles,
  conflict_wait_secs: latencyPercentiles,
  total_weight_unit_minutes: number,
  avg_ci_runs_before_merge: nullable(number),
  avg_review_rounds: nullable(number),
  runs_with_tokens: withDefault(integer, 0),
  tokens_in_total: nullable(integer),
  cached_tokens_total: nullable(integer),
  tokens_out_total: nullable(integer),
  reasoning_tokens_total: nullable(integer),
  token_cache_hit_rate: nullable(number),
  tokens_by_kind: withDefault(record(kindTokenStats), {}),
  per_device: withDefault(array(deviceSchedulerStats), []),
  per_board: withDefault(array(boardSchedulerStats), []),
  per_policy_version: withDefault(array(policyVersionStats), []),
  holds_by_reason: withDefault(record(integer), {}),
  latest_evaluation: nullable(schedulerEvaluation),
});

export const HEALTH_POSTURES = ["healthy", "degraded", "incident"] as const;
export const ALERT_SEVERITIES = ["info", "warning", "critical"] as const;
export const SERVICE_STATES = ["running", "stopped", "failed", "degraded"] as const;
export const BACKUP_STATES = ["ok", "stale", "failed"] as const;
export const SCHEDULE_SOURCES = [
  "launchd",
  "systemd",
  "t3",
  "cron",
  "github",
  "board",
  "other",
] as const;
export const SCHEDULE_CATEGORIES = [
  "backup",
  "monitoring",
  "agent_ops",
  "sync",
  "security",
  "maintenance",
  "research",
  "app",
  "os",
  "other",
] as const;
export const SCHEDULE_STATES = [
  "ok",
  "running",
  "failing",
  "overdue",
  "never_run",
  "disabled",
] as const;

export const hostResources = object({
  cpu_pct: nullable(number),
  load_1m: nullable(number),
  load_5m: nullable(number),
  load_15m: nullable(number),
  mem_used_gib: nullable(number),
  mem_total_gib: nullable(number),
  disk_used_gib: nullable(number),
  disk_total_gib: nullable(number),
  disk_free_gib: nullable(number),
  disk_days_to_full: nullable(number),
  slice_cpu_cap_pct: nullable(number),
});

export const hostNetworkStatus = object({
  tailscale_online: nullable(boolean),
  tailscale_mode: nullable(string),
  tailscale_latency_ms: nullable(number),
  heartbeat_ok: nullable(boolean),
  heartbeat_checked_at: nullable(timestamp),
  heartbeat_label: nullable(string),
});

export const containerStatus = object({
  name: string,
  image: nullable(string),
  state: string,
  health: nullable(string),
  uptime_secs: nullable(integer),
  restarts: nullable(integer),
  status_text: nullable(string),
});

export const serviceStatus = object({
  name: string,
  kind: string,
  state: oneOfOrUnknown(...SERVICE_STATES),
  pid: nullable(integer),
  memory_mib: nullable(number),
  uptime_secs: nullable(integer),
  restarts: nullable(integer),
  detail: nullable(string),
});

export const hostStorageQuota = object({
  host: string,
  name: string,
  backend: nullable(string),
  used_gib: number,
  quota_gib: number,
  warn_pct: nullable(number),
  detail: nullable(string),
});

export const hostBackupStatus = object({
  host: string,
  target: string,
  last_success_at: nullable(timestamp),
  sla_secs: nullable(integer),
  state: oneOfOrUnknown(...BACKUP_STATES),
  detail: nullable(string),
});

export const hostExpiryItem = object({
  host: string,
  name: string,
  kind: string,
  expires_at: nullable(timestamp),
  warn_days: nullable(integer),
  ok: withDefault(boolean, true),
  detail: nullable(string),
});

export const hostExternalAlert = object({
  host_reporter: string,
  name: string,
  severity: withDefault(oneOfOrUnknown(...ALERT_SEVERITIES), "info"),
  host: nullable(string),
  summary: string,
  active_since: nullable(timestamp),
  is_watchdog: withDefault(boolean, false),
  source: nullable(string),
});

export const hostVaultHealth = object({
  host: string,
  reachable: boolean,
  sealed: boolean,
  ca_valid_until: nullable(timestamp),
  audit_ok: nullable(boolean),
  agents_running: nullable(integer),
  detail: nullable(string),
});

export const hostCiHealth = object({
  host: string,
  poller_fresh: nullable(boolean),
  last_poll_at: nullable(timestamp),
  webhook_errors: nullable(integer),
  runners_online: nullable(integer),
  runners_busy: nullable(integer),
  egress_ok: nullable(boolean),
  detail: nullable(string),
});

export const incidentCounts = object({
  firing_alerts: integer,
  watchdog_ok: nullable(boolean),
  failing_jobs: integer,
  overdue_jobs: integer,
  stale_hosts: integer,
  version_skews: integer,
  open_asks: integer,
  stale_leases: integer,
  open_feedback: integer,
});

export const serverHealthSummary = object({
  version: string,
  commit: string,
  schema_version: integer,
  uptime_secs: integer,
  db_bytes: nullable(integer),
  wal_bytes: nullable(integer),
  features: withDefault(array(string), []),
});

export const queueCounts = object({
  ready_auto: integer,
  ready_manual: integer,
  doing: integer,
  review: integer,
  blocked: integer,
});

export const deployTrainStatus = object({
  board: string,
  repo: nullable(string),
  pending_tasks: withDefault(array(string), []),
  oldest_ready_at: nullable(timestamp),
  flush_at: nullable(timestamp),
  active_lock_holder: nullable(string),
  last_deploy_at: nullable(timestamp),
  last_deploy_state: nullable(string),
  last_deploy_sha: nullable(string),
});

export const leaseHygieneItem = object({
  id: integer,
  agent: string,
  task: nullable(string),
  repo: string,
  glob: string,
  expires_at: timestamp,
  stale_holder: boolean,
});

export const leaseHygieneSummary = object({
  active_count: integer,
  stale_holder_count: integer,
  items: withDefault(array(leaseHygieneItem), []),
});

export const goalCooldownStatus = object({
  board: string,
  autopilot: boolean,
  every_secs: nullable(integer),
  last_goal_at: nullable(timestamp),
  next_eligible_at: nullable(timestamp),
  cooldown_remaining_secs: nullable(integer),
});

export const factoryHealthSummary = object({
  active_runs: integer,
  queue: queueCounts,
  runs_total: integer,
  runs_succeeded: integer,
  runs_failed: integer,
  runs_orphaned: integer,
  first_pass_merge_rate: nullable(number),
  avg_review_rounds: nullable(number),
  deploy_trains: withDefault(array(deployTrainStatus), []),
  leases: leaseHygieneSummary,
  goal_cooldowns: withDefault(array(goalCooldownStatus), []),
  open_feedback: integer,
});

export const dailyTokenBucket = object({
  day: string,
  runs: integer,
  tokens_in: integer,
  billable_in: integer,
  cached_tokens: integer,
  tokens_out: integer,
  cache_hit_rate: nullable(number),
});

export const tokenBreakdownRow = object({
  key: string,
  runs: integer,
  tokens_in: integer,
  billable_in: integer,
  cached_tokens: integer,
  tokens_out: integer,
  cache_hit_rate: nullable(number),
  avg_billable_per_run: integer,
});

export const tokenAnalyticsSummary = object({
  days: integer,
  runs_with_tokens: integer,
  tokens_in: integer,
  billable_in: integer,
  cached_tokens: integer,
  tokens_out: integer,
  reasoning_tokens: integer,
  cache_hit_rate: nullable(number),
  daily: withDefault(array(dailyTokenBucket), []),
  by_board: withDefault(array(tokenBreakdownRow), []),
  by_kind: withDefault(array(tokenBreakdownRow), []),
  by_device: withDefault(array(tokenBreakdownRow), []),
  by_model: withDefault(array(tokenBreakdownRow), []),
});

export const hostResourceSample = object({
  hour_bucket: timestamp,
  cpu_pct: nullable(number),
  mem_used_pct: nullable(number),
  disk_used_pct: nullable(number),
  disk_free_gib: nullable(number),
});

export const hostHealthSummary = object({
  host: string,
  device_role: nullable(oneOfOrUnknown(...DEVICE_ROLES)),
  status: oneOfOrUnknown(...DEVICE_STATUSES),
  os: nullable(string),
  arch: nullable(string),
  uptime_secs: nullable(integer),
  reported_at: nullable(timestamp),
  collectors: withDefault(array(string), []),
  resources: nullable(hostResources),
  network: nullable(hostNetworkStatus),
  versions: withDefault(record(string), {}),
  containers: withDefault(array(containerStatus), []),
  services: withDefault(array(serviceStatus), []),
  samples: withDefault(array(hostResourceSample), []),
});

export const hostVersionEntry = object({
  host: string,
  version: string,
});

export const versionSkew = object({
  component: string,
  expected: string,
  mismatched: withDefault(array(hostVersionEntry), []),
});

export const healthOverview = object({
  generated_at: timestamp,
  days: integer,
  posture: oneOfOrUnknown(...HEALTH_POSTURES),
  incidents: incidentCounts,
  server: serverHealthSummary,
  factory: factoryHealthSummary,
  tokens: tokenAnalyticsSummary,
  hosts: withDefault(array(hostHealthSummary), []),
  storage: withDefault(array(hostStorageQuota), []),
  backups: withDefault(array(hostBackupStatus), []),
  certs: withDefault(array(hostExpiryItem), []),
  alerts: withDefault(array(hostExternalAlert), []),
  vault: withDefault(array(hostVaultHealth), []),
  ci: withDefault(array(hostCiHealth), []),
  version_skews: withDefault(array(versionSkew), []),
  devices: withDefault(array(deviceEntry), []),
  usage_buckets: withDefault(array(usageBucket), []),
});

export const healthRefreshFailure = object({
  device: string,
  error: string,
});

export const healthRefreshResponse = object({
  triggered: withDefault(array(string), []),
  failed: withDefault(array(healthRefreshFailure), []),
});

export const scheduledJobSnapshot = object({
  job_key: string,
  host: string,
  source: oneOfOrUnknown(...SCHEDULE_SOURCES),
  id: string,
  name: string,
  category: oneOfOrUnknown(...SCHEDULE_CATEGORIES),
  enabled: boolean,
  schedule_human: string,
  schedule_raw: string,
  expected_interval_secs: nullable(integer),
  state: oneOfOrUnknown(...SCHEDULE_STATES),
  last_run_at: nullable(timestamp),
  next_run_at: nullable(timestamp),
  last_duration_ms: nullable(integer),
  last_exit_code: nullable(integer),
  run_count: nullable(integer),
  last_error: nullable(string),
  command_preview: nullable(string),
  deep_link: nullable(string),
  host_stale: withDefault(boolean, false),
  reported_at: timestamp,
  updated_at: timestamp,
});

export const scheduleSummary = object({
  total: integer,
  failing: integer,
  overdue: integer,
  running: integer,
  ok: integer,
  never_run: integer,
  disabled: integer,
  os_hidden: integer,
});

export const scheduleListResponse = object({
  generated_at: timestamp,
  summary: scheduleSummary,
  hosts: withDefault(array(string), []),
  jobs: withDefault(array(scheduledJobSnapshot), []),
});

export const scheduledJobHistoryEntry = object({
  id: integer,
  at: timestamp,
  state: oneOfOrUnknown(...SCHEDULE_STATES),
  exit_code: nullable(integer),
  duration_ms: nullable(integer),
  error_summary: nullable(string),
  run_count: nullable(integer),
});

export const scheduleHistoryResponse = object({
  job_key: string,
  entries: withDefault(array(scheduledJobHistoryEntry), []),
});

export type Board = Decoded<typeof board>;
export type BoardKind = Board["kind"];
export type StatusCounts = Decoded<typeof statusCounts>;
export type Task = Decoded<typeof task>;
export type TaskOrigin = Task["origin"];
export type TaskSize = Task["size"];
export type RankedTasks = Decoded<typeof rankedTasks>;
export type DispatchHoldReason = Decoded<typeof dispatchHoldReason>;
export type Feedback = NonNullable<Task["feedback"]>;
export type TaskStatus = Task["status"];
export type Priority = Task["priority"];
export type TaskDetail = Decoded<typeof taskDetail>;
export type ChildTask = Decoded<typeof childTask>;
export type Note = Decoded<typeof note>;
export type Evidence = Decoded<typeof evidence>;
export type Agent = Decoded<typeof agent>;
export type Handoff = Decoded<typeof handoff>;
export type Ask = Decoded<typeof ask>;
/** One entry of the event log. Named to avoid the DOM's `Event`. */
export type BoardEvent = Decoded<typeof event>;
export type EventsPage = Decoded<typeof eventsPage>;
export type Health = Decoded<typeof health>;
export type Said = Decoded<typeof said>;
export type ConflictLock = Decoded<typeof conflictLock>;
export type Run = Decoded<typeof run>;
export type RunKind = Run["kind"];
export type RunOrigin = Run["origin"];
export type RunState = Run["state"];
export type RunEvent = Decoded<typeof runEvent>;
export type RunEventKind = RunEvent["kind"];
export type SteerRunResponse = Decoded<typeof steerRunResponse>;
export type CancelRunResponse = Decoded<typeof cancelRunResponse>;
export type Taint = Decoded<typeof taint>;
export type TaintEffect = Taint["effect"];
export type DeviceCapacity = Decoded<typeof deviceCapacity>;
export type DeviceEntry = Decoded<typeof deviceEntry>;
export type DeviceRole = DeviceEntry["role"];
export type DeviceStatus = DeviceEntry["status"];
export type UsageWindow = Decoded<typeof usageWindow>;
export type UsagePace = Decoded<typeof usagePace>;
export type UsageBucket = Decoded<typeof usageBucket>;
export type SchedulerWeights = Decoded<typeof schedulerWeights>;
export type SchedulerPolicy = Decoded<typeof schedulerPolicy>;
export type SchedulerPolicyResponse = Decoded<typeof schedulerPolicyResponse>;
export type LatencyPercentiles = Decoded<typeof latencyPercentiles>;
export type DeviceSchedulerStats = Decoded<typeof deviceSchedulerStats>;
export type BoardSchedulerStats = Decoded<typeof boardSchedulerStats>;
export type PolicyVersionStats = Decoded<typeof policyVersionStats>;
export type ReplayMetrics = Decoded<typeof replayMetrics>;
export type SchedulerEvaluation = Decoded<typeof schedulerEvaluation>;
export type SchedulerEvaluationsResponse = Decoded<typeof schedulerEvaluationsResponse>;
export type AutotuneMode = SchedulerEvaluation["mode"];
export type AutotuneDecision = SchedulerEvaluation["decision"];
export type KindTokenStats = Decoded<typeof kindTokenStats>;
export type SchedulerStats = Decoded<typeof schedulerStats>;
export type HealthPosture = HealthOverview["posture"];
export type AlertSeverity = HostExternalAlert["severity"];
export type ServiceState = ServiceStatus["state"];
export type BackupState = HostBackupStatus["state"];
export type ScheduleSource = ScheduledJobSnapshot["source"];
export type ScheduleCategory = ScheduledJobSnapshot["category"];
export type ScheduleState = ScheduledJobSnapshot["state"];
export type HostResources = Decoded<typeof hostResources>;
export type HostNetworkStatus = Decoded<typeof hostNetworkStatus>;
export type ContainerStatus = Decoded<typeof containerStatus>;
export type ServiceStatus = Decoded<typeof serviceStatus>;
export type HostStorageQuota = Decoded<typeof hostStorageQuota>;
export type HostBackupStatus = Decoded<typeof hostBackupStatus>;
export type HostExpiryItem = Decoded<typeof hostExpiryItem>;
export type HostExternalAlert = Decoded<typeof hostExternalAlert>;
export type HostVaultHealth = Decoded<typeof hostVaultHealth>;
export type HostCiHealth = Decoded<typeof hostCiHealth>;
export type IncidentCounts = Decoded<typeof incidentCounts>;
export type ServerHealthSummary = Decoded<typeof serverHealthSummary>;
export type QueueCounts = Decoded<typeof queueCounts>;
export type DeployTrainStatus = Decoded<typeof deployTrainStatus>;
export type LeaseHygieneItem = Decoded<typeof leaseHygieneItem>;
export type LeaseHygieneSummary = Decoded<typeof leaseHygieneSummary>;
export type GoalCooldownStatus = Decoded<typeof goalCooldownStatus>;
export type FactoryHealthSummary = Decoded<typeof factoryHealthSummary>;
export type DailyTokenBucket = Decoded<typeof dailyTokenBucket>;
export type TokenBreakdownRow = Decoded<typeof tokenBreakdownRow>;
export type TokenAnalyticsSummary = Decoded<typeof tokenAnalyticsSummary>;
export type HostResourceSample = Decoded<typeof hostResourceSample>;
export type HostHealthSummary = Decoded<typeof hostHealthSummary>;
export type VersionSkew = Decoded<typeof versionSkew>;
export type HealthOverview = Decoded<typeof healthOverview>;
export type HealthRefreshResponse = Decoded<typeof healthRefreshResponse>;
export type ScheduledJobSnapshot = Decoded<typeof scheduledJobSnapshot>;
export type ScheduleSummary = Decoded<typeof scheduleSummary>;
export type ScheduleListResponse = Decoded<typeof scheduleListResponse>;
export type ScheduledJobHistoryEntry = Decoded<typeof scheduledJobHistoryEntry>;
export type ScheduleHistoryResponse = Decoded<typeof scheduleHistoryResponse>;
