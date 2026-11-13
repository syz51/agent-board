// Which cached data an event makes stale. Each query in the web view declares tags; an
// event invalidates the tags below, and the cache refetches the queries that carry them.
// The web view keeps no replica of server state: it refetches what an event touches.

import type { BoardEvent } from "../api/schema.ts";

export const tags = {
  boards: "boards",
  agents: "agents",
  asks: "asks",
  /** Tasks across boards, such as the agents screen's current tasks. */
  tasks: "tasks",
  /** Every list of what a coordinator said at the end of its turns (the agents screen). */
  said: "said",
  board: (slug: string) => `board:${slug}`,
  task: (id: string) => `task:${id}`,
  handoff: (agentId: string) => `handoff:${agentId}`,
  /** One agent's said list: a turn ending changes only the list of the agent it ends. */
  saidOf: (agentId: string) => `said:${agentId}`,
} as const;

/** Lease and file-touch events change nothing the web view shows (they appear in the feed). */
const IGNORED = new Set(["lease_created", "lease_released", "lease_expired", "files_touched"]);

/** A board's deletion is recorded as `board_updated` with `fields: ["deleted"]`. */
function isDeletion(event: BoardEvent): boolean {
  const fields = (event.payload as { fields?: unknown } | null)?.fields;
  return Array.isArray(fields) && fields.includes("deleted");
}

export function tagsForEvent(event: BoardEvent): string[] {
  if (IGNORED.has(event.type)) return [];

  const out: string[] = [];
  const onBoard = (...extra: string[]) => {
    if (event.board) out.push(tags.board(event.board));
    if (event.task) out.push(tags.task(event.task));
    out.push(...extra);
  };

  switch (event.type) {
    case "board_created":
      onBoard(tags.boards);
      break;
    case "board_updated":
      if (isDeletion(event)) {
        // Its tasks, asks and memberships went with it.
        onBoard(tags.boards, tags.tasks, tags.agents, tags.asks, tags.said);
      } else {
        onBoard(tags.boards);
      }
      break;
    case "board_joined":
      onBoard(tags.boards, tags.agents);
      break;
    case "task_created":
    case "task_claimed":
    case "task_released":
    case "task_taken":
    case "task_updated":
    case "task_done":
      // Counts on the boards screen and current tasks on the agents screen change too.
      onBoard(tags.boards, tags.tasks);
      break;
    case "note_added":
    case "evidence_added":
      if (event.task) out.push(tags.task(event.task));
      break;
    case "handoff_updated":
      out.push(tags.handoff(event.actor));
      break;
    case "agent_registered":
      // The boards screen shows the coordinator's state.
      out.push(tags.agents, tags.boards);
      break;
    case "agent_state_changed":
      // A coordinator's turn ending is stored on the same Stop that changes its state: its
      // own said list changes, no other agent's.
      out.push(tags.agents, tags.boards, tags.saidOf(event.actor));
      break;
    case "ask_created":
    case "ask_answered":
    case "ask_withdrawn":
      out.push(tags.asks, tags.boards);
      if (event.task) out.push(tags.task(event.task));
      break;
    default:
      // A type this view does not know yet: refresh everything it could touch.
      onBoard(tags.boards, tags.tasks, tags.agents, tags.asks, tags.said);
  }
  return out;
}

export interface Timers {
  setTimeout(callback: () => void, ms: number): unknown;
  clearTimeout(handle: unknown): void;
}

export const browserTimers: Timers = {
  setTimeout: (callback, ms) => globalThis.setTimeout(callback, ms),
  clearTimeout: (handle) => globalThis.clearTimeout(handle as number),
};

/**
 * Collects tags from a burst of events (an agent finishing a task emits several) and
 * flushes them once, so one burst costs one refetch per affected query.
 */
export class TagBatcher {
  private pending = new Set<string>();
  private timer: unknown = null;

  constructor(
    private readonly flush: (tags: Set<string>) => void,
    private readonly delayMs = 150,
    private readonly timers: Timers = browserTimers,
  ) {}

  add(eventTags: Iterable<string>): void {
    for (const tag of eventTags) this.pending.add(tag);
    if (this.pending.size === 0 || this.timer !== null) return;
    this.timer = this.timers.setTimeout(() => {
      this.timer = null;
      const batch = this.pending;
      this.pending = new Set();
      this.flush(batch);
    }, this.delayMs);
  }
}
