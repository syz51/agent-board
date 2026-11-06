// Wires the event stream to the cache for the running page, and exposes Preact hooks.

import { useCallback, useEffect, useMemo, useState } from "preact/hooks";
import { api } from "../api/client.ts";
import type { BoardEvent } from "../api/schema.ts";
import { type ResourceState, ResourceCache } from "./cache.ts";
import { browserTimers, TagBatcher, tagsForEvent } from "./refresh.ts";
import { type ConnectionState, EventStream, openEventSource } from "./stream.ts";

export const cache = new ResourceCache();

const batcher = new TagBatcher((batch) => cache.invalidate(batch));
const eventListeners = new Set<(event: BoardEvent) => void>();
const stateListeners = new Set<(state: ConnectionState) => void>();
let connection: ConnectionState = { kind: "connecting" };

export const stream = new EventStream(
  {
    ...browserTimers,
    openSource: openEventSource,
    initialSeq: async () => (await api.health()).seq,
    now: () => Date.now(),
    isOnline: () => navigator.onLine,
  },
  {
    onEvent(event) {
      batcher.add(tagsForEvent(event));
      for (const listener of eventListeners) listener(event);
    },
    onState(state) {
      connection = state;
      for (const listener of stateListeners) listener(state);
    },
    onResync: () => cache.invalidateAll(),
  },
);

export function startLive(): void {
  addEventListener("online", () => stream.online());
  addEventListener("offline", () => stream.offline());
  stream.start();
}

export interface Resource<T> extends ResourceState<T> {
  retry(): void;
}

/**
 * Read a query through the cache. `key` identifies the query; `queryTags` say which events
 * make it stale. Returns the last good data while refreshing or after a failed refresh.
 */
export function useResource<T>(
  key: string | null,
  queryTags: readonly string[],
  fetcher: () => Promise<T>,
): Resource<T> {
  const [, setTick] = useState(0);
  const rerender = useCallback(() => setTick((n) => n + 1), []);
  const tagKey = queryTags.join("|");

  useEffect(() => {
    if (key === null) return;
    return cache.subscribe(key, queryTags, fetcher, rerender);
    // The key names the query; fetcher and tags follow from it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, tagKey]);

  const state = key === null
    ? { data: undefined, error: undefined, loading: false }
    : cache.read<T>(key);
  // Before the first subscribe runs, a query with no data counts as loading.
  const loading = state.loading || (key !== null && state.data === undefined && !state.error);
  return useMemo(
    () => ({ ...state, loading, retry: () => key !== null && cache.refresh(key) }),
    [state, loading, key],
  );
}

export function useConnection(): ConnectionState {
  const [state, setState] = useState(connection);
  useEffect(() => {
    stateListeners.add(setState);
    setState(connection);
    return () => void stateListeners.delete(setState);
  }, []);
  return state;
}

/** Live events as they arrive, for the feed. */
export function useLiveEvents(listener: (event: BoardEvent) => void): void {
  useEffect(() => {
    eventListeners.add(listener);
    return () => void eventListeners.delete(listener);
  }, [listener]);
}

/** The current time, updated every `ms`, for relative times. */
export function useNow(ms = 30_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(timer);
  }, [ms]);
  return now;
}
