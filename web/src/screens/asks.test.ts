import { describe, expect, test } from "bun:test";
import { DevState } from "../../dev/state.ts";
import type { Task } from "../api/schema.ts";
import { needsYouCount, waitingTasks } from "./asks.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const candidates = () => [
  ...state.tasks(new URLSearchParams({ status: "open" })),
  ...state.tasks(new URLSearchParams({ status: "blocked" })),
];
const openAsks = () => state.asks(new URLSearchParams({ status: "open" }));
const ids = (tasks: Task[], asks = openAsks(), at = now) =>
  Object.fromEntries(
    waitingTasks(tasks, asks, at).map((group) => [group.board, group.tasks.map((task) => task.id)]),
  );

describe("waitingTasks", () => {
  test("lists blocked and non-auto open tasks without an open ask, grouped by board", () => {
    expect(ids(candidates())).toEqual({
      // docs-4 is a feedback item, docs-6 waits for its not_before.
      docs: ["docs-5"],
      // factory-17 is blocked but has open ask #7.
      factory: ["factory-18"],
      "release-engineering": ["release-engineering-112"],
      // web-view-4 is a pending proposal.
      "web-view": ["web-view-2"],
    });
    expect(waitingTasks(candidates(), openAsks(), now).map((group) => group.board)).toEqual([
      "docs",
      "factory",
      "release-engineering",
      "web-view",
    ]);
  });

  test("a blocked task shows once its ask is answered", () => {
    const asks = openAsks().filter((ask) => ask.task !== "factory-17");
    expect(ids(candidates(), asks).factory).toEqual(["factory-17", "factory-18"]);
  });

  test("a non-auto task held by a not_before shows once the date passes", () => {
    const docs6 = candidates().find((task) => task.id === "docs-6");
    const after = Date.parse(docs6?.not_before ?? "") + 1;
    expect(ids(candidates(), openAsks(), after).docs).toEqual(["docs-5", "docs-6"]);
  });

  test("nothing waits when every such task is auto, asked or a proposal", () => {
    expect(ids(candidates().filter((task) => task.auto && task.status === "open"))).toEqual({});
  });
});

describe("needsYouCount", () => {
  test("counts the open asks plus the tasks the page lists, on every board", () => {
    // Asks #7, #8 and #9; docs-5, factory-18, release-engineering-112 and web-view-2.
    expect(needsYouCount(openAsks(), candidates(), now)).toBe(7);
    const listed = waitingTasks(candidates(), openAsks(), now).flatMap((group) => group.tasks);
    expect(needsYouCount(openAsks(), candidates(), now)).toBe(openAsks().length + listed.length);
  });

  test("an answered ask leaves the count; its blocked task joins it", () => {
    const asks = openAsks().filter((ask) => ask.id !== 7);
    // Ask #7 goes, factory-17 (blocked, no ask now) comes.
    expect(needsYouCount(asks, candidates(), now)).toBe(7);
    expect(
      needsYouCount(
        asks.filter((ask) => ask.id !== 8),
        candidates(),
        now,
      ),
    ).toBe(6);
  });

  test("tasks on closed boards are left out", () => {
    expect(ids(candidates()).docs).toEqual(["docs-5"]);
    const closed = new Set(["docs"]);
    expect(waitingTasks(candidates(), openAsks(), now, closed).map((group) => group.board)).toEqual(
      ["factory", "release-engineering", "web-view"],
    );
    expect(needsYouCount(openAsks(), candidates(), now, closed)).toBe(6);
  });

  test("before the tasks load, it counts the open asks", () => {
    expect(needsYouCount(openAsks(), [], now)).toBe(3);
    expect(needsYouCount([], [], now)).toBe(0);
  });
});

describe("pending proposals", () => {
  const base = candidates().find((task) => task.id === "web-view-4") as Task;

  test("the dev server marks an open non-auto task whose trimmed body starts with Proposal:", () => {
    expect(base.body.startsWith("Proposal:")).toBe(false);
    expect(base.proposal).toBe(true);
    expect(base.ready).toBe(false);
  });

  test("a blocked proposal without an ask still waits on the user", () => {
    // The server clears `proposal` once the task is no longer open.
    const blocked = { ...base, status: "blocked" as const, proposal: false };
    expect(ids([blocked], [])).toEqual({ "web-view": ["web-view-4"] });
  });
});
