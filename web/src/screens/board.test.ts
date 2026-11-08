import { describe, expect, test } from "bun:test";
import { DevState } from "../../dev/state.ts";
import { groupTasks, showsTasks, type TaskFilters } from "./board.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const tasks = new DevState(() => now).tasks(new URLSearchParams({ board: "factory" }));
const none: TaskFilters = { q: "", priority: "", owner: "", readyOnly: false };
const ids = (filters: Partial<TaskFilters> = {}) =>
  Object.fromEntries(
    groupTasks(tasks, { ...none, ...filters }, now).map((group) => [
      group.key,
      group.tasks.map((task) => task.id),
    ]),
  );

describe("groupTasks", () => {
  test("groups by status, ready apart from waiting, done of the last 30 days only, no dropped", () => {
    expect(ids()).toEqual({
      doing: ["factory-12", "factory-14"],
      review: ["factory-13"],
      blocked: ["factory-17"],
      ready: ["factory-15", "factory-16", "factory-18", "factory-20", "factory-21"],
      open: ["factory-19", "factory-30"],
      done: ["factory-9", "factory-8"],
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
