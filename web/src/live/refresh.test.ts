import { describe, expect, test } from "bun:test";
import { ApiFailure } from "../api/client.ts";
import type { BoardEvent } from "../api/schema.ts";
import type { QueryClient } from "@tanstack/react-query";
import { QueryObserver } from "@tanstack/react-query";
import {
  addToFeed,
  createQueryClient,
  fetching,
  invalidateTags,
  mergeEvents,
  queries,
} from "./query.ts";
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
    expect(tagsForEvent(event("note_added", { task: "factory-12" }))).toEqual([
      tags.task("factory-12"),
    ]);
    expect(tagsForEvent(event("evidence_added", { task: "factory-9" }))).toEqual([
      tags.task("factory-9"),
    ]);
  });

  test("a handoff refreshes the writing agent's handoff", () => {
    expect(tagsForEvent(event("handoff_updated", { actor: "t3:c2d0" }))).toEqual([
      tags.handoff("t3:c2d0"),
    ]);
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
    ).toEqual([
      tags.board("factory"),
      tags.boards,
      tags.tasks,
      tags.agents,
      tags.asks,
      tags.updates,
    ]);
  });

  test("agent changes refresh agents and boards, which show the coordinator", () => {
    expect(tagsForEvent(event("agent_registered", { board: null }))).toEqual([
      tags.agents,
      tags.boards,
    ]);
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

describe("query tags", () => {
  /** A shown query: an observer keeps it active, as a mounted screen does. */
  function show<T>(
    client: QueryClient,
    key: string,
    queryTags: string[],
    fetcher: () => Promise<T>,
  ) {
    const observer = new QueryObserver(client, {
      queryKey: [key],
      queryFn: fetching(fetcher),
      meta: { tags: queryTags },
    });
    const unsubscribe = observer.subscribe(() => {});
    return { observer, unsubscribe, read: () => observer.getCurrentResult() };
  }

  test("refetches a shown query when one of its tags is invalidated, and only then", async () => {
    const client = createQueryClient();
    let calls = 0;
    const shown = show(client, "tasks:factory", [tags.board("factory")], async () => ++calls);
    await flush();
    expect(shown.read().data).toBe(1);

    await invalidateTags(client, [tags.board("web-view"), tags.asks]);
    expect(calls).toBe(1);

    await invalidateTags(client, [tags.board("factory")]);
    expect(shown.read().data).toBe(2);
  });

  test("a query nobody shows is marked stale and refetched when shown again", async () => {
    const client = createQueryClient();
    let calls = 0;
    const fetcher = async () => ++calls;
    show(client, "boards", [tags.boards], fetcher).unsubscribe();
    await flush();

    await invalidateTags(client, [tags.boards]);
    expect(calls).toBe(1);

    const again = show(client, "boards", [tags.boards], fetcher);
    // The stale data stays visible while the refetch runs.
    expect(again.read()).toMatchObject({ data: 1, isFetching: true });
    await flush();
    expect(again.read().data).toBe(2);
  });

  test("a fresh query is not refetched when another screen shows it", async () => {
    const client = createQueryClient();
    let calls = 0;
    const fetcher = async () => ++calls;
    show(client, "agents", [tags.agents], fetcher);
    await flush();
    show(client, "agents", [tags.agents], fetcher);
    await flush();
    expect(calls).toBe(1);
  });

  test("an invalidation during a refresh cancels it and fetches again, so the last event wins", async () => {
    const client = createQueryClient();
    const answers = [deferred<string>(), deferred<string>(), deferred<string>()];
    let call = 0;
    const shown = show(client, "asks", [tags.asks], () => answers[call++]!.promise);
    answers[0]!.resolve("first load");
    await flush();
    void invalidateTags(client, [tags.asks]);
    void invalidateTags(client, [tags.asks]);
    await flush();
    expect(call).toBe(3);
    answers[2]!.resolve("after the event");
    answers[1]!.resolve("before the event");
    await flush();
    expect(shown.read().data).toBe("after the event");
  });

  test("an invalidation during the first load fetches again once it settles", async () => {
    const client = createQueryClient();
    const answers = [deferred<string>(), deferred<string>()];
    let call = 0;
    const shown = show(client, "asks", [tags.asks], () => answers[call++]!.promise);
    await flush();
    void invalidateTags(client, [tags.asks]);
    await flush();
    expect(call).toBe(1);

    answers[0]!.resolve("before the event");
    await flush();
    expect(call).toBe(2);
    answers[1]!.resolve("after the event");
    await flush();
    expect(shown.read().data).toBe("after the event");
  });

  test("an invalidation during a hidden query's first load leaves it stale", async () => {
    const client = createQueryClient();
    const first = deferred<string>();
    let calls = 0;
    const shown = show(client, "asks", [tags.asks], () =>
      ++calls === 1 ? first.promise : Promise.resolve("fresh"),
    );
    await flush();
    shown.unsubscribe();
    void invalidateTags(client, [tags.asks]);
    first.resolve("before the event");
    await flush();
    expect(calls).toBe(1);
    expect(client.getQueryState(["asks"])?.isInvalidated).toBe(true);
  });

  test("a failed refresh keeps the last good data beside the error", async () => {
    const client = createQueryClient();
    let fail = false;
    const shown = show(client, "boards", [tags.boards], async () => {
      if (fail) throw new ApiFailure(500, "internal", "db locked", "retry");
      return ["factory"];
    });
    await flush();
    fail = true;
    await invalidateTags(client, [tags.boards]);
    expect(shown.read()).toMatchObject({ data: ["factory"], isFetching: false });
    expect(shown.read().error?.message).toBe("db locked");

    fail = false;
    await shown.observer.refetch();
    expect(shown.read().error).toBeNull();
  });

  test("failures that are not API failures still arrive as one, with what to do next", async () => {
    const client = createQueryClient();
    const original = console.error;
    console.error = () => {};
    try {
      const shown = show(client, "boom", [], async () => {
        throw new TypeError("x is undefined");
      });
      await flush();
      expect(shown.read().error).toBeInstanceOf(ApiFailure);
      expect(shown.read().error?.message).toBe("The web view failed: x is undefined");
    } finally {
      console.error = original;
    }
  });

  test("a burst of events during the first load fetches again only once", async () => {
    const client = createQueryClient();
    const first = deferred<string>();
    let calls = 0;
    show(client, "asks", [tags.asks], () =>
      ++calls === 1 ? first.promise : Promise.resolve("after the burst"),
    );
    await flush();
    for (let i = 0; i < 5; i++) void invalidateTags(client, [tags.asks]);
    first.resolve("before the burst");
    await flush();
    await flush();
    expect(calls).toBe(2);
  });

  test("invalidating every query refetches every shown one after a reconnect", async () => {
    const client = createQueryClient();
    const counts = { a: 0, b: 0 };
    show(client, "a", ["x"], async () => ++counts.a);
    show(client, "b", ["y"], async () => ++counts.b);
    await flush();
    await client.invalidateQueries();
    expect(counts).toEqual({ a: 2, b: 2 });
  });
});

describe("event feed", () => {
  test("a live event joins a loaded feed, newest first, once", () => {
    const client = createQueryClient();
    const key = queries.events().queryKey;
    addToFeed(client, event("task_done", { seq: 5 }));
    expect(client.getQueryData(key)).toBeUndefined();

    client.setQueryData(key, [event("task_created", { seq: 3 })]);
    addToFeed(client, event("task_done", { seq: 5 }));
    addToFeed(client, event("task_done", { seq: 5 }));
    addToFeed(client, event("note_added", { seq: 4 }));
    expect(client.getQueryData(key)?.map((item) => item.seq)).toEqual([5, 4, 3]);
  });

  test("a live event during the feed's first load joins it once the load lands", async () => {
    const client = createQueryClient();
    const key = queries.events().queryKey;
    const page = deferred<BoardEvent[]>();
    const observer = new QueryObserver(client, {
      queryKey: key,
      queryFn: () => page.promise,
      meta: { tags: [] },
    });
    observer.subscribe(() => {});
    await flush();
    addToFeed(client, event("task_done", { seq: 5 }));
    page.resolve([event("task_created", { seq: 3 })]);
    await flush();
    expect(client.getQueryData(key)?.map((item) => item.seq)).toEqual([5, 3]);
  });

  test("a live event during a failed first load is left to the retry", async () => {
    const client = createQueryClient();
    const key = queries.events().queryKey;
    const page = deferred<BoardEvent[]>();
    const observer = new QueryObserver(client, {
      queryKey: key,
      queryFn: () => page.promise,
      meta: { tags: [] },
    });
    observer.subscribe(() => {});
    await flush();
    addToFeed(client, event("task_done", { seq: 5 }));
    page.reject(new ApiFailure(500, "internal", "db locked", "retry"));
    await flush();
    expect(client.getQueryData(key)).toBeUndefined();
    expect(observer.getCurrentResult().error?.message).toBe("db locked");
  });

  test("merging keeps older pages and live events, ordered by sequence", () => {
    const merged = mergeEvents(
      [event("a", { seq: 9 }), event("b", { seq: 7 })],
      [event("c", { seq: 8 })],
    );
    expect(merged.map((item) => item.seq)).toEqual([9, 8, 7]);
  });
});
