import { describe, expect, test } from "bun:test";
import { ApiFailure } from "../api/client.ts";
import type { BoardEvent } from "../api/schema.ts";
import { ResourceCache } from "./cache.ts";
import { TagBatcher, tags, tagsForEvent } from "./refresh.ts";
import { manualTimers } from "./testing.ts";

function event(type: string, fields: Partial<BoardEvent> = {}): BoardEvent {
  return {
    seq: 1,
    at: "2026-11-06T12:00:00Z",
    board: "factory",
    actor: "t3:7f3a",
    type,
    task: null,
    payload: {},
    ...fields,
  };
}

/** A promise settled from outside, for controlling fetch order. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("tagsForEvent", () => {
  test("a task change refreshes its board, the task, board counts and current tasks", () => {
    expect(tagsForEvent(event("task_claimed", { task: "factory-12" }))).toEqual([
      tags.board("factory"),
      tags.task("factory-12"),
      tags.boards,
      tags.tasks,
    ]);
  });

  test("notes and evidence refresh only the task", () => {
    expect(tagsForEvent(event("note_added", { task: "factory-12" }))).toEqual([tags.task("factory-12")]);
    expect(tagsForEvent(event("evidence_added", { task: "factory-9" }))).toEqual([tags.task("factory-9")]);
  });

  test("a handoff refreshes the writing agent's handoff", () => {
    expect(tagsForEvent(event("handoff_updated", { actor: "t3:c2d0" }))).toEqual([tags.handoff("t3:c2d0")]);
  });

  test("a board change refreshes the board and the boards list", () => {
    expect(tagsForEvent(event("board_updated", { payload: { fields: ["status"] } }))).toEqual([
      tags.board("factory"),
      tags.boards,
    ]);
  });

  test("a board's deletion also refreshes the tasks, agents and asks that went with it", () => {
    expect(
      tagsForEvent(event("board_updated", { payload: { fields: ["deleted"], title: "Factory" } })),
    ).toEqual([tags.board("factory"), tags.boards, tags.tasks, tags.agents, tags.asks, tags.updates]);
  });

  test("agent changes refresh agents and boards, which show the coordinator", () => {
    expect(tagsForEvent(event("agent_registered", { board: null }))).toEqual([tags.agents, tags.boards]);
  });

  test("an agent's state change also refreshes updates: a coordinator's turn ending comes with it", () => {
    expect(tagsForEvent(event("agent_state_changed", { board: null }))).toEqual([
      tags.agents,
      tags.boards,
      tags.updates,
    ]);
  });

  test("done and updated tasks refresh updates; claims and releases do not", () => {
    for (const type of ["task_done", "task_updated"]) {
      expect(tagsForEvent(event(type, { task: "factory-9" }))).toContain(tags.updates);
    }
    for (const type of ["task_created", "task_claimed", "task_released", "task_taken"]) {
      expect(tagsForEvent(event(type, { task: "factory-9" }))).not.toContain(tags.updates);
    }
  });

  test("a plain board change, notes, asks and handoffs leave updates alone", () => {
    const quiet = [
      event("board_updated", { payload: { fields: ["autopilot"] } }),
      event("note_added", { task: "factory-9" }),
      event("ask_created"),
      event("handoff_updated"),
    ];
    for (const item of quiet) expect(tagsForEvent(item)).not.toContain(tags.updates);
  });

  test("ask changes refresh asks, board counts and the task", () => {
    expect(tagsForEvent(event("ask_answered", { task: "factory-17" }))).toEqual([
      tags.asks,
      tags.boards,
      tags.task("factory-17"),
    ]);
  });

  test("lease and file events change nothing shown outside the feed", () => {
    for (const type of ["lease_created", "lease_released", "lease_expired", "files_touched"]) {
      expect(tagsForEvent(event(type, { task: "factory-12" }))).toEqual([]);
    }
  });

  test("an unknown event type refreshes everything it might touch", () => {
    expect(tagsForEvent(event("future_thing", { task: "factory-1" }))).toEqual([
      tags.board("factory"),
      tags.task("factory-1"),
      tags.boards,
      tags.tasks,
      tags.agents,
      tags.asks,
      tags.updates,
    ]);
  });
});

describe("TagBatcher", () => {
  test("flushes a burst of events once, with the union of their tags", () => {
    const clock = manualTimers();
    const flushed: string[][] = [];
    const batcher = new TagBatcher((batch) => flushed.push([...batch].sort()), 150, clock.timers);
    batcher.add(["a", "b"]);
    batcher.add(["b", "c"]);
    expect(clock.pending.size).toBe(1);
    expect(flushed).toEqual([]);
    clock.runAll();
    expect(flushed).toEqual([["a", "b", "c"]]);
    batcher.add(["d"]);
    clock.runAll();
    expect(flushed).toEqual([["a", "b", "c"], ["d"]]);
  });

  test("an event with no tags schedules nothing", () => {
    const clock = manualTimers();
    new TagBatcher(() => {}, 150, clock.timers).add([]);
    expect(clock.pending.size).toBe(0);
  });
});

describe("ResourceCache", () => {
  test("refetches a shown query when one of its tags is invalidated, and only then", async () => {
    const cache = new ResourceCache();
    let calls = 0;
    const fetcher = async () => ++calls;
    cache.subscribe("tasks:factory", [tags.board("factory")], fetcher, () => {});
    await flush();
    expect(cache.read("tasks:factory").data).toBe(1);

    cache.invalidate([tags.board("web-view"), tags.asks]);
    await flush();
    expect(calls).toBe(1);

    cache.invalidate([tags.board("factory")]);
    await flush();
    expect(cache.read("tasks:factory").data).toBe(2);
  });

  test("a query nobody shows is marked stale and refetched when shown again", async () => {
    const cache = new ResourceCache();
    let calls = 0;
    const fetcher = async () => ++calls;
    const unsubscribe = cache.subscribe("boards", [tags.boards], fetcher, () => {});
    await flush();
    unsubscribe();

    cache.invalidate([tags.boards]);
    await flush();
    expect(calls).toBe(1);

    cache.subscribe("boards", [tags.boards], fetcher, () => {});
    // The stale data stays visible while the refetch runs.
    expect(cache.read("boards")).toMatchObject({ data: 1, loading: true });
    await flush();
    expect(cache.read("boards").data).toBe(2);
  });

  test("a fresh query is not refetched when another screen subscribes to it", async () => {
    const cache = new ResourceCache();
    let calls = 0;
    const fetcher = async () => ++calls;
    cache.subscribe("agents", [tags.agents], fetcher, () => {});
    await flush();
    cache.subscribe("agents", [tags.agents], fetcher, () => {});
    await flush();
    expect(calls).toBe(1);
  });

  test("an invalidation during a fetch fetches again afterwards, so the last event wins", async () => {
    const cache = new ResourceCache();
    const answers = [deferred<string>(), deferred<string>()];
    let call = 0;
    cache.subscribe("asks", [tags.asks], () => answers[call++]!.promise, () => {});
    cache.invalidate([tags.asks]);
    cache.invalidate([tags.asks]);
    expect(call).toBe(1);

    answers[0]!.resolve("before the event");
    await flush();
    expect(call).toBe(2);
    answers[1]!.resolve("after the event");
    await flush();
    expect(cache.read("asks").data).toBe("after the event");
  });

  test("a failed refresh keeps the last good data beside the error", async () => {
    const cache = new ResourceCache();
    let fail = false;
    cache.subscribe("boards", [tags.boards], async () => {
      if (fail) throw new ApiFailure(500, "internal", "db locked", "retry");
      return ["factory"];
    }, () => {});
    await flush();
    fail = true;
    cache.invalidate([tags.boards]);
    await flush();
    expect(cache.read("boards")).toMatchObject({ data: ["factory"], loading: false });
    expect(cache.read("boards").error?.message).toBe("db locked");

    fail = false;
    cache.refresh("boards");
    await flush();
    expect(cache.read("boards").error).toBeUndefined();
  });

  test("invalidateAll refetches every shown query after a reconnect", async () => {
    const cache = new ResourceCache();
    const counts = { a: 0, b: 0 };
    cache.subscribe("a", ["x"], async () => ++counts.a, () => {});
    cache.subscribe("b", ["y"], async () => ++counts.b, () => {});
    await flush();
    cache.invalidateAll();
    await flush();
    expect(counts).toEqual({ a: 2, b: 2 });
  });

  test("notifies subscribers on every state change", async () => {
    const cache = new ResourceCache();
    let notified = 0;
    cache.subscribe("boards", [tags.boards], async () => [], () => notified++);
    await flush();
    // loading, then loaded.
    expect(notified).toBe(2);
  });
});
