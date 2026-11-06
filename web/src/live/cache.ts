// A small keyed cache of server reads. Screens subscribe to a key with its tags and fetcher;
// invalidating a tag refetches the subscribed queries that carry it and marks the others
// stale, so they refetch when a screen shows them again.

import { ApiFailure } from "../api/client.ts";

export interface ResourceState<T> {
  data: T | undefined;
  error: ApiFailure | undefined;
  /** A fetch is in flight (the first load, or a refresh behind existing data). */
  loading: boolean;
}

interface Entry {
  state: ResourceState<unknown>;
  tags: ReadonlySet<string>;
  fetcher: () => Promise<unknown>;
  listeners: Set<() => void>;
  inflight: boolean;
  /** Invalidated while a fetch was in flight: fetch again when it settles. */
  again: boolean;
  stale: boolean;
}

const IDLE: ResourceState<never> = { data: undefined, error: undefined, loading: false };

export class ResourceCache {
  private entries = new Map<string, Entry>();

  read<T>(key: string): ResourceState<T> {
    return (this.entries.get(key)?.state ?? IDLE) as ResourceState<T>;
  }

  /** Subscribe to a query; fetches it when it has no data yet or is stale. */
  subscribe<T>(
    key: string,
    queryTags: readonly string[],
    fetcher: () => Promise<T>,
    listener: () => void,
  ): () => void {
    let entry = this.entries.get(key);
    if (!entry) {
      entry = {
        state: IDLE,
        tags: new Set(queryTags),
        fetcher,
        listeners: new Set(),
        inflight: false,
        again: false,
        stale: true,
      };
      this.entries.set(key, entry);
    } else {
      entry.fetcher = fetcher;
      entry.tags = new Set(queryTags);
    }
    entry.listeners.add(listener);
    if (!entry.inflight && (entry.stale || entry.state.error)) void this.load(entry);
    const current = entry;
    return () => current.listeners.delete(listener);
  }

  /** Refetch now. */
  refresh(key: string): void {
    const entry = this.entries.get(key);
    if (entry) void this.load(entry);
  }

  invalidate(eventTags: Iterable<string>): void {
    const wanted = new Set(eventTags);
    for (const entry of this.entries.values()) {
      if ([...entry.tags].some((tag) => wanted.has(tag))) this.markStale(entry);
    }
  }

  /** After a reconnect, or an event the view could not read: everything may have changed. */
  invalidateAll(): void {
    for (const entry of this.entries.values()) this.markStale(entry);
  }

  private markStale(entry: Entry): void {
    entry.stale = true;
    if (entry.listeners.size > 0) void this.load(entry);
  }

  private async load(entry: Entry): Promise<void> {
    if (entry.inflight) {
      entry.again = true;
      return;
    }
    entry.inflight = true;
    entry.stale = false;
    this.set(entry, { ...entry.state, loading: true });
    try {
      const data = await entry.fetcher();
      this.set(entry, { data, error: undefined, loading: false });
    } catch (error) {
      // Keep the last good data on screen; the error is shown beside it.
      this.set(entry, { data: entry.state.data, error: toFailure(error), loading: false });
    } finally {
      entry.inflight = false;
    }
    if (entry.again) {
      entry.again = false;
      await this.load(entry);
    }
  }

  private set(entry: Entry, state: ResourceState<unknown>): void {
    entry.state = state;
    for (const listener of entry.listeners) listener();
  }
}

function toFailure(error: unknown): ApiFailure {
  if (error instanceof ApiFailure) return error;
  // A bug in this view rather than a server answer: surface it instead of hiding it.
  console.error(error);
  const message = error instanceof Error ? error.message : String(error);
  return new ApiFailure(0, "client_error", `The web view failed: ${message}`, "Reload the page.");
}
