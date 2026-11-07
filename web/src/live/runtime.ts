// The running page's live wiring: the event stream invalidates query tags and feeds the
// event list; React reads the connection state and the clock through hooks.

import { useEffect, useState, useSyncExternalStore } from "react";
import { api } from "@/api/client.ts";
import { addToFeed, createQueryClient, invalidateTags } from "./query.ts";
import { browserTimers, TagBatcher, tagsForEvent } from "./refresh.ts";
import { type ConnectionState, EventStream, openEventSource } from "./stream.ts";

export const queryClient = createQueryClient();

const batcher = new TagBatcher((batch) => void invalidateTags(queryClient, batch));
const connectionListeners = new Set<() => void>();
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
      addToFeed(queryClient, event);
    },
    onState(state) {
      connection = state;
      for (const listener of connectionListeners) listener();
    },
    // Events may have been missed: refetch everything shown, mark the rest stale.
    onResync: () => void queryClient.invalidateQueries(),
  },
);

export function startLive(): void {
  addEventListener("online", () => stream.online());
  addEventListener("offline", () => stream.offline());
  stream.start();
}

function subscribeConnection(listener: () => void): () => void {
  connectionListeners.add(listener);
  return () => connectionListeners.delete(listener);
}

const readConnection = () => connection;

export function useConnection(): ConnectionState {
  return useSyncExternalStore(subscribeConnection, readConnection, readConnection);
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
