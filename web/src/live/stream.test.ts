import { describe, expect, test } from "bun:test";
import type { BoardEvent } from "../api/schema.ts";
import { manualTimers } from "./testing.ts";
import { BACKOFF_MS, type ConnectionState, EventStream, type SourceHandlers } from "./stream.ts";

interface FakeSource {
  url: string;
  handlers: SourceHandlers;
  closed: boolean;
}

function setup(options: { initialSeq?: () => Promise<number> } = {}) {
  const clock = manualTimers();
  const sources: FakeSource[] = [];
  const events: BoardEvent[] = [];
  const states: ConnectionState[] = [];
  let resyncs = 0;
  let online = true;
  const stream = new EventStream(
    {
      ...clock.timers,
      openSource(url, handlers) {
        const source: FakeSource = { url, handlers, closed: false };
        sources.push(source);
        return { close: () => void (source.closed = true) };
      },
      initialSeq: options.initialSeq ?? (async () => 100),
      now: () => 0,
      isOnline: () => online,
    },
    {
      onEvent: (event) => events.push(event),
      onState: (state) => states.push(state),
      onResync: () => resyncs++,
    },
  );
  return {
    stream,
    clock,
    sources,
    events,
    states,
    get resyncs() {
      return resyncs;
    },
    setOnline(value: boolean) {
      online = value;
    },
    last() {
      const source = sources.at(-1);
      if (!source) throw new Error("no source opened");
      return source;
    },
  };
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

function message(seq: number, type = "task_updated") {
  return JSON.stringify({ seq, at: "2026-11-06T12:00:00Z", board: "factory", actor: "t3:7f3a", type, task: "factory-12", payload: {} });
}

describe("EventStream", () => {
  test("starts from the server's latest sequence and goes live", async () => {
    const t = setup();
    t.stream.start();
    await flush();
    expect(t.last().url).toBe("/v1/events?since=100");
    expect(t.states.at(-1)).toEqual({ kind: "connecting" });
    t.last().handlers.open();
    expect(t.states.at(-1)).toEqual({ kind: "live" });
    // The first connection is not a reconnect.
    expect(t.resyncs).toBe(0);
  });

  test("delivers events in order and drops replays it has already seen", async () => {
    const t = setup();
    t.stream.start();
    await flush();
    t.last().handlers.open();
    for (const seq of [101, 102, 102, 101, 103]) t.last().handlers.message(message(seq));
    expect(t.events.map((event) => event.seq)).toEqual([101, 102, 103]);
    expect(t.stream.seq).toBe(103);
  });

  test("after an error it backs off, reconnects from the last sequence and resyncs", async () => {
    const t = setup();
    t.stream.start();
    await flush();
    t.last().handlers.open();
    t.last().handlers.message(message(107));

    const first = t.last();
    first.handlers.error();
    expect(first.closed).toBe(true);
    expect(t.states.at(-1)).toEqual({ kind: "reconnecting", attempt: 1, retryAt: BACKOFF_MS[0] });
    expect([...t.clock.pending.values()][0]?.ms).toBe(BACKOFF_MS[0]);

    t.clock.runAll();
    await flush();
    expect(t.last().url).toBe("/v1/events?since=107");
    t.last().handlers.open();
    expect(t.states.at(-1)).toEqual({ kind: "live" });
    // Anything shown may have changed while disconnected.
    expect(t.resyncs).toBe(1);
  });

  test("backs off further on repeated failures, then holds at the longest delay", async () => {
    const t = setup();
    t.stream.start();
    await flush();
    const delays: number[] = [];
    for (let i = 0; i < BACKOFF_MS.length + 2; i++) {
      t.last().handlers.error();
      delays.push([...t.clock.pending.values()][0]?.ms ?? -1);
      t.clock.runAll();
      await flush();
    }
    expect(delays).toEqual([...BACKOFF_MS, BACKOFF_MS.at(-1)!, BACKOFF_MS.at(-1)!]);
  });

  test("ignores callbacks from a source it already replaced", async () => {
    const t = setup();
    t.stream.start();
    await flush();
    const old = t.last();
    old.handlers.error();
    t.clock.runAll();
    await flush();
    old.handlers.message(message(150));
    old.handlers.error();
    expect(t.events).toEqual([]);
    expect(t.clock.pending.size).toBe(0);
  });

  test("goes offline with the browser and reconnects at once when it is back", async () => {
    const t = setup();
    t.stream.start();
    await flush();
    t.last().handlers.open();
    const live = t.last();

    t.setOnline(false);
    t.stream.offline();
    expect(live.closed).toBe(true);
    expect(t.states.at(-1)).toEqual({ kind: "offline" });

    t.setOnline(true);
    t.stream.online();
    await flush();
    expect(t.sources.length).toBe(2);
    t.last().handlers.open();
    expect(t.resyncs).toBe(1);
  });

  test("an unreadable message asks for a resync instead of being dropped silently", async () => {
    const t = setup();
    t.stream.start();
    await flush();
    t.last().handlers.open();
    const error = console.error;
    console.error = () => {};
    try {
      t.last().handlers.message("{not json");
      t.last().handlers.message(JSON.stringify({ seq: "x" }));
    } finally {
      console.error = error;
    }
    expect(t.resyncs).toBe(2);
    expect(t.events).toEqual([]);
  });

  test("retries when the server is unreachable before the first connection", async () => {
    let attempts = 0;
    const t = setup({
      initialSeq: async () => {
        attempts++;
        if (attempts === 1) throw new Error("unreachable");
        return 40;
      },
    });
    t.stream.start();
    await flush();
    expect(t.states.at(-1)?.kind).toBe("reconnecting");
    expect(t.sources.length).toBe(0);
    t.clock.runAll();
    await flush();
    expect(t.last().url).toBe("/v1/events?since=40");
  });

  test("stop closes the source and cancels a pending retry", async () => {
    const t = setup();
    t.stream.start();
    await flush();
    t.last().handlers.error();
    t.stream.stop();
    expect(t.clock.pending.size).toBe(0);
  });
});
