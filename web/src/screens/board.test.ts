import { describe, expect, test } from "bun:test";
import { DevState } from "../../dev/state.ts";
import { groupTasks, showsTasks, type TaskFilters } from "./board.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const tasks = new DevState(() => now).tasks(new URLSearchParams({ board: "factory" }));
const none: TaskFilters = { q: "", priority: "", owner: "", readyOnly: false, parent: "" };
const ids = (filters: Partial<TaskFilters> = {}) =>
  Object.fromEntries(
    groupTasks(tasks, { ...none, ...filters }, now).map((group) => [
      group.key,
      group.tasks.map((task) => task.id),
    ]),
  );

describe("groupTasks", () => {
  test("ranked tasks lead their priority level in rank order, the rest follow by number", () => {
    const ranks: Record<string, number> = { "factory-21": 1, "factory-16": 2 };
    const ranked = tasks.map((task) => ({
      ...task,
      priority: task.id === "factory-20" ? ("p1" as const) : ("p2" as const),
      rank: ranks[task.id] ?? null,
    }));
    const ready = groupTasks(ranked.toReversed(), none, now).find((group) => group.key === "ready");
    expect(ready?.tasks.map((task) => task.id)).toEqual([
      "factory-20",
      "factory-21",
      "factory-16",
      "factory-15",
      "factory-18",
    ]);
  });

  test("groups by status, ready apart from waiting, done of the last 30 days only, no dropped", () => {
    expect(ids()).toEqual({
      doing: ["factory-12", "factory-14"],
      review: ["factory-13"],
      blocked: ["factory-17"],
      ready: ["factory-15", "factory-16", "factory-18", "factory-20", "factory-21"],
      open: ["factory-19", "factory-30"],
      done: ["factory-9", "factory-8"],
      dropped: [],
    });
    expect(tasks.find((task) => task.id === "factory-19")?.ready).toBe(false);
  });

  test("a task waiting for its not_before is not ready until then", () => {
    let clock = now;
    const state = new DevState(() => clock);
    const waiting = () =>
      state
        .tasks(new URLSearchParams({ board: "factory" }))
        .find((task) => task.id === "factory-30");
    const notBefore = waiting()?.not_before;
    expect(notBefore).toBeString();
    expect(waiting()?.ready).toBe(false);
    clock = Date.parse(notBefore ?? "") + 1;
    expect(waiting()?.ready).toBe(true);
  });

  test("filters by priority, owner, readiness and text", () => {
    expect(ids({ priority: "p1" }).ready).toEqual(["factory-15"]);
    expect(ids({ owner: "none" }).doing).toEqual([]);
    expect(ids({ readyOnly: true }).open).toEqual([]);
    expect(ids({ q: "LEASES" }).ready).toEqual(["factory-15"]);
    expect(ids({ q: "factory-13" }).review).toEqual(["factory-13"]);
  });

  test("narrows to a parent's direct children, with every done one", () => {
    expect(ids({ parent: "factory-12" })).toEqual({
      doing: [],
      review: ["factory-13"],
      blocked: ["factory-17"],
      ready: ["factory-21"],
      open: ["factory-19"],
      // factory-3 was done over 30 days ago: the board list leaves it out, its parent not.
      done: ["factory-8", "factory-3"],
      dropped: [],
    });
    expect(ids().done).not.toContain("factory-3");
    // It combines with the other filters; a task without children leaves nothing.
    expect(ids({ parent: "factory-12", priority: "p1" }).blocked).toEqual(["factory-17"]);
    expect(ids({ parent: "factory-12", priority: "p1" }).review).toEqual([]);
    expect(Object.values(ids({ parent: "factory-21" })).flat()).toEqual([]);
  });

  test("shows a parent's dropped children, which the whole board leaves out", () => {
    const base = tasks.find((task) => task.id === "factory-15")!;
    const dropped = { ...base, id: "factory-99", status: "dropped" as const, parent: "factory-12" };
    const groups = (filters: Partial<TaskFilters>) =>
      groupTasks([...tasks, dropped], { ...none, ...filters }, now).find(
        (group) => group.key === "dropped",
      )?.tasks;
    expect(groups({ parent: "factory-12" })?.map((task) => task.id)).toEqual(["factory-99"]);
    expect(groups({})).toEqual([]);
  });
});

describe("showsTasks", () => {
  const at = (id: string) => {
    const task = tasks.find((item) => item.id === id);
    if (!task) throw new Error(`no fixture task ${id}`);
    return task;
  };

  test("counts only tasks the list would show before filtering", () => {
    expect(showsTasks(tasks, now)).toBe(true);
    expect(showsTasks([], now)).toBe(false);
    const dropped = { ...at("factory-15"), status: "dropped" as const };
    const oldDone = { ...at("factory-9"), updated_at: "2026-09-01T00:00:00Z" };
    expect(showsTasks([dropped, oldDone], now)).toBe(false);
    expect(showsTasks([dropped, at("factory-9")], now)).toBe(true);
  });
});
