// Server state lives in TanStack Query. Each query declares tags in `meta.tags`; an event
// invalidates the tags it touches (see refresh.ts), which refetches the shown queries that
// carry one and marks the others stale, so they refetch when a screen shows them again.
// Data stays fresh until an event says otherwise: no polling, no refetch on focus.

import { type Query, QueryClient, queryOptions } from "@tanstack/react-query";
import { ApiFailure, api } from "@/api/client.ts";
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

/** First loads that already fetch again once they settle, so a burst of events asks once. */
const awaitingFirstLoad = new WeakSet<Query>();

/**
 * Refetch the shown queries carrying any of these tags; mark the rest stale. A refetch
 * cancels one in flight, so the last event wins; a first load is not cancelled, so it is
 * invalidated again once it settles.
 */
export function invalidateTags(client: QueryClient, eventTags: Iterable<string>): Promise<void> {
  const wanted = new Set(eventTags);
  const predicate = (query: Query) => (query.meta?.tags ?? []).some((tag) => wanted.has(tag));
  for (const query of client.getQueryCache().findAll({ predicate, fetchStatus: "fetching" })) {
    if (query.state.data !== undefined || awaitingFirstLoad.has(query)) continue;
    awaitingFirstLoad.add(query);
    const again = () => {
      awaitingFirstLoad.delete(query);
      void client.invalidateQueries({ queryKey: query.queryKey, exact: true });
    };
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

/** Tasks in progress on every board, doing then review: the home page's In progress. */
async function activeTasks() {
  const [doing, review] = await Promise.all([
    api.tasks({ status: "doing" }),
    api.tasks({ status: "review" }),
  ]);
  return [...doing, ...review];
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
  activeTasks: () =>
    queryOptions({
      queryKey: ["tasks", "active"],
      queryFn: fetching(() => activeTasks()),
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
};
