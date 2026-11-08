import { describe, expect, test } from "bun:test";
import { DevState } from "../../dev/state.ts";
import type { Task } from "../api/schema.ts";
import { newestFirst } from "./asks.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const openAsks = () => state.asks(new URLSearchParams({ status: "open" }));

describe("newestFirst", () => {
  test("orders asks newest first, the higher number first on a tie", () => {
    const asks = openAsks();
    const ordered = newestFirst(asks);
    expect(ordered.length).toBe(asks.length);
    for (let i = 1; i < ordered.length; i++) {
      const [a, b] = [ordered[i - 1], ordered[i]];
      if (!a || !b) throw new Error("missing ask");
      const order = Date.parse(a.created_at) - Date.parse(b.created_at) || a.id - b.id;
      expect(order).toBeGreaterThan(0);
    }
    const tie = { ...ordered[0]!, id: 999 };
    expect(newestFirst([ordered[0]!, tie])[0]?.id).toBe(999);
  });
});

describe("pending proposals", () => {
  test("the dev server marks an open non-auto task whose trimmed body starts with Proposal:", () => {
    const task = state
      .tasks(new URLSearchParams({ status: "open" }))
      .find((item) => item.id === "web-view-4") as Task;
    expect(task.body.startsWith("Proposal:")).toBe(false);
    expect(task.proposal).toBe(true);
    expect(task.ready).toBe(false);
  });
});
