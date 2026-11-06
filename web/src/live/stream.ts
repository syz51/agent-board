// The live connection: an EventSource on `/v1/events`, reopened from the last sequence
// number after any failure. Reconnects are managed here rather than by the browser's
// built-in retry, so every reconnect resumes with an explicit `since` and a visible state.

import { DecodeError } from "../api/decode.ts";
import { type BoardEvent, event as decodeEvent } from "../api/schema.ts";
import type { Timers } from "./refresh.ts";

export type ConnectionState =
  | { kind: "connecting" }
  | { kind: "live" }
  | { kind: "reconnecting"; attempt: number; retryAt: number }
  | { kind: "offline" };

export interface SourceHandlers {
  open(): void;
  message(data: string): void;
  error(): void;
}

/** Opens an `EventSource` (or a test double) wired to the handlers; returns its closer. */
export type OpenSource = (url: string, handlers: SourceHandlers) => { close(): void };

export const openEventSource: OpenSource = (url, handlers) => {
  const source = new EventSource(url);
  source.onopen = () => handlers.open();
  source.onmessage = (message: MessageEvent<string>) => handlers.message(message.data);
  source.onerror = () => handlers.error();
  return source;
};

export interface StreamDeps extends Timers {
  openSource: OpenSource;
  /** The latest sequence number, read once before the first connection. */
  initialSeq(): Promise<number>;
  now(): number;
  isOnline(): boolean;
}

export interface StreamHandlers {
  onEvent(event: BoardEvent): void;
  onState(state: ConnectionState): void;
  /** Events may have been missed or could not be read: refetch everything shown. */
  onResync(): void;
}

/** Delay before reconnect attempt n (1-based); the last value repeats. */
export const BACKOFF_MS = [1_000, 2_000, 5_000, 10_000, 15_000] as const;

export class EventStream {
  private source: { close(): void } | null = null;
  private retryTimer: unknown = null;
  private lastSeq: number | null = null;
  private attempt = 0;
  private stopped = true;
  private state: ConnectionState = { kind: "connecting" };

  constructor(
    private readonly deps: StreamDeps,
    private readonly handlers: StreamHandlers,
  ) {}

  get seq(): number | null {
    return this.lastSeq;
  }

  start(): void {
    if (!this.stopped) return;
    this.stopped = false;
    void this.connect();
  }

  stop(): void {
    this.stopped = true;
    this.clearRetry();
    this.closeSource();
  }

  /** The browser went offline: drop the connection and wait for `online()`. */
  offline(): void {
    if (this.stopped) return;
    this.clearRetry();
    this.closeSource();
    // Coming back is a reconnect: resync what was shown.
    this.attempt = Math.max(this.attempt, 1);
    this.setState({ kind: "offline" });
  }

  /** The browser is back online: reconnect now instead of waiting for the backoff. */
  online(): void {
    if (this.stopped || this.state.kind === "live" || this.state.kind === "connecting") return;
    this.clearRetry();
    void this.connect();
  }

  private async connect(): Promise<void> {
    if (this.stopped) return;
    if (!this.deps.isOnline()) {
      this.setState({ kind: "offline" });
      return;
    }
    if (this.attempt === 0) this.setState({ kind: "connecting" });
    else if (this.state.kind === "offline") {
      this.setState({ kind: "reconnecting", attempt: this.attempt, retryAt: this.deps.now() });
    }

    if (this.lastSeq === null) {
      try {
        this.lastSeq = await this.deps.initialSeq();
      } catch {
        // The server is unreachable; the reconnect state says so and retries.
        this.scheduleRetry();
        return;
      }
      if (this.stopped) return;
    }

    const source = this.deps.openSource(`/v1/events?since=${this.lastSeq}`, {
      open: () => {
        if (this.source !== source) return;
        const reconnected = this.attempt > 0;
        this.attempt = 0;
        this.setState({ kind: "live" });
        if (reconnected) this.handlers.onResync();
      },
      message: (data) => {
        if (this.source === source) this.receive(data);
      },
      error: () => {
        if (this.source !== source) return;
        this.closeSource();
        this.scheduleRetry();
      },
    });
    this.source = source;
  }

  private receive(data: string): void {
    let event: BoardEvent;
    try {
      event = decodeEvent(JSON.parse(data), "event");
    } catch (error) {
      if (!(error instanceof SyntaxError || error instanceof DecodeError)) throw error;
      console.error("Unreadable event from /v1/events", error);
      this.handlers.onResync();
      return;
    }
    // Replays after a reconnect can repeat events already seen.
    if (this.lastSeq !== null && event.seq <= this.lastSeq) return;
    this.lastSeq = event.seq;
    this.handlers.onEvent(event);
  }

  private scheduleRetry(): void {
    if (this.stopped) return;
    if (!this.deps.isOnline()) {
      this.setState({ kind: "offline" });
      return;
    }
    this.attempt += 1;
    const delay = BACKOFF_MS[Math.min(this.attempt, BACKOFF_MS.length) - 1] ?? 15_000;
    this.setState({ kind: "reconnecting", attempt: this.attempt, retryAt: this.deps.now() + delay });
    this.retryTimer = this.deps.setTimeout(() => {
      this.retryTimer = null;
      void this.connect();
    }, delay);
  }

  private clearRetry(): void {
    if (this.retryTimer !== null) this.deps.clearTimeout(this.retryTimer);
    this.retryTimer = null;
  }

  private closeSource(): void {
    this.source?.close();
    this.source = null;
  }

  private setState(state: ConnectionState): void {
    this.state = state;
    this.handlers.onState(state);
  }
}
