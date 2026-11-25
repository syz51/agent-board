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

const boardStatus = oneOf("active", "paused", "closed");
const taskStatus = oneOf(...TASK_STATUSES);
const priority = oneOf(...PRIORITIES);

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
  goal_review_every_secs: nullable(integer),
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
  /** The task's place inside its priority level, lower first; none follows the ranked. */
  rank: nullable(integer),
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

export const RUN_KINDS = [
  "worker",
  "review",
  "verdict",
  "triage",
  "goal",
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

export type Board = Decoded<typeof board>;
export type StatusCounts = Decoded<typeof statusCounts>;
export type Task = Decoded<typeof task>;
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
