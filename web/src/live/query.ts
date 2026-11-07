// Server state lives in TanStack Query. Each query declares tags in `meta.tags`; an event
// invalidates the tags it touches (see refresh.ts), which refetches the shown queries that
// carry one and marks the others stale, so they refetch when a screen shows them again.
// Data stays fresh until an event says otherwise: no polling, no refetch on focus.

import { type Query, QueryClient, queryOptions } from "@tanstack/react-query";
import { ApiFailure, api } from "@/api/client.ts";
import type { BoardEvent } from "@/api/schema.ts";
import { boardOfTask } from "@/lib/ids.ts";
import { tags } from "./refresh.ts";

declare module "@tanstack/react-query" {
  interface Register {
    defaultError: ApiFailure;
    queryMeta: { tags: readonly string[] };
    mutationMeta: Record<string, never>;
  }
}

export function createQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        // Events decide staleness; a failed query shows its error with a retry button.
        staleTime: Infinity,
        gcTime: Infinity,
        retry: false,
        refetchOnWindowFocus: false,
        refetchOnReconnect: false,
      },
      mutations: { retry: false },
    },
  });
}

/**
 * Refetch the shown queries carrying any of these tags; mark the rest stale. A refetch
 * cancels one in flight, so the last event wins; a first load is not cancelled, so it is
 * invalidated again once it settles.
 */
export function invalidateTags(client: QueryClient, eventTags: Iterable<string>): Promise<void> {
  const wanted = new Set(eventTags);
  const predicate = (query: Query) => (query.meta?.tags ?? []).some((tag) => wanted.has(tag));
  for (const query of client.getQueryCache().findAll({ predicate, fetchStatus: "fetching" })) {
    if (query.state.data !== undefined) continue;
    const again = () => void client.invalidateQueries({ queryKey: query.queryKey, exact: true });
    query.promise?.then(again, again);
  }
  return client.invalidateQueries({ predicate });
}

/** Every failure as an `ApiFailure`; anything else is a bug in this view, surfaced as one. */
export function toFailure(error: unknown): ApiFailure {
  if (error instanceof ApiFailure) return error;
  console.error(error);
  const message = error instanceof Error ? error.message : String(error);
  return new ApiFailure(0, "client_error", `The web view failed: ${message}`, "Reload the page.");
}

/** A query or mutation function whose failures are all `ApiFailure`s. */
export function fetching<A extends unknown[], T>(
  fetcher: (...args: A) => Promise<T>,
): (...args: A) => Promise<T> {
  return async (...args) => {
    try {
      return await fetcher(...args);
    } catch (error) {
      throw toFailure(error);
    }
  };
}

/** Events by sequence number, newest first. */
export function mergeEvents(
  current: readonly BoardEvent[],
  incoming: readonly BoardEvent[],
): BoardEvent[] {
  const bySeq = new Map(current.map((event) => [event.seq, event]));
  for (const event of incoming) bySeq.set(event.seq, event);
  return [...bySeq.values()].sort((a, b) => b.seq - a.seq);
}

/** Events per page of the feed. */
export const EVENTS_PAGE = 100;

/** Open and blocked tasks on every board: Needs you and its navigation count. */
async function candidateTasks() {
  const [open, blocked] = await Promise.all([
    api.tasks({ status: "open" }),
    api.tasks({ status: "blocked" }),
  ]);
  return [...open, ...blocked];
}

export const queries = {
  boards: () =>
    queryOptions({
      queryKey: ["boards"],
      queryFn: fetching(() => api.boards()),
      meta: { tags: [tags.boards] },
    }),
  boardTasks: (board: string) =>
    queryOptions({
      queryKey: ["tasks", "board", board],
      queryFn: fetching(() => api.tasks({ board })),
      meta: { tags: [tags.board(board)] },
    }),
  doingTasks: () =>
    queryOptions({
      queryKey: ["tasks", "doing"],
      queryFn: fetching(() => api.tasks({ status: "doing" })),
      meta: { tags: [tags.tasks] },
    }),
  candidateTasks: () =>
    queryOptions({
      queryKey: ["tasks", "needs-you"],
      queryFn: fetching(() => candidateTasks()),
      meta: { tags: [tags.tasks] },
    }),
  task: (id: string) =>
    queryOptions({
      queryKey: ["task", id],
      queryFn: fetching(() => api.task(id)),
      meta: { tags: [tags.task(id), tags.board(boardOfTask(id))] },
    }),
  agents: () =>
    queryOptions({
      queryKey: ["agents"],
      queryFn: fetching(() => api.agents()),
      meta: { tags: [tags.agents] },
    }),
  handoff: (agentId: string) =>
    queryOptions({
      queryKey: ["handoff", agentId],
      queryFn: fetching(() => api.handoff(agentId)),
      meta: { tags: [tags.handoff(agentId)] },
    }),
  said: (agentId: string, limit: number) =>
    queryOptions({
      queryKey: ["said", agentId, limit],
      queryFn: fetching(() => api.agentSaid(agentId, limit)),
      meta: { tags: [tags.updates] },
    }),
  openAsks: () =>
    queryOptions({
      queryKey: ["asks", "open"],
      queryFn: fetching(() => api.asks({ status: "open" })),
      meta: { tags: [tags.asks] },
    }),
  ask: (id: number) =>
    queryOptions({
      queryKey: ["ask", id],
      queryFn: fetching(() => api.ask(id)),
      meta: { tags: [tags.asks] },
    }),
  updates: (board: string) =>
    queryOptions({
      queryKey: ["updates", board],
      queryFn: fetching(() => api.updates(board ? { board } : {})),
      meta: { tags: [tags.updates] },
    }),
  /**
   * The event feed: the latest page merged into what is already shown. Live events are
   * added by the runtime as they arrive; older pages are added on demand.
   */
  events: () =>
    queryOptions({
      queryKey: ["events"],
      queryFn: fetching(async () => {
        const { seq } = await api.health();
        const page = await api.events({
          since: Math.max(0, seq - EVENTS_PAGE),
          limit: EVENTS_PAGE,
        });
        return page.events;
      }),
      // Keep events already shown (live ones, older pages) when the latest page is refetched.
      structuralSharing: (previous, next) =>
        mergeEvents((previous as BoardEvent[] | undefined) ?? [], next as BoardEvent[]),
      meta: { tags: [] },
    }),
};

/** Add a live event to the feed, if the feed has loaded; its first load includes it anyway. */
export function addToFeed(client: QueryClient, event: BoardEvent): void {
  const key = queries.events().queryKey;
  if (client.getQueryData(key) === undefined) return;
  client.setQueryData(key, (current) => mergeEvents(current ?? [], [event]));
}
