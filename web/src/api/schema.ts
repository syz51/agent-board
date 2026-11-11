// Wire types of `crates/board-proto` (model.rs, api.rs), as decoders. The TypeScript types
// are derived from the decoders so the two cannot drift apart.

import {
  array,
  boolean,
  type Decoded,
  integer,
  json,
  nullable,
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
  blocked_reason: nullable(string),
  owner: nullable(string),
  owner_stale: boolean,
  blockers: array(string),
  ready: boolean,
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
  runtime: oneOf("claude", "codex", "opencode"),
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
