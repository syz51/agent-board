import { describe, expect, test } from "bun:test";
import { DevState } from "../../dev/state.ts";
import { groupTasks, type TaskFilters } from "./board.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const tasks = new DevState(() => now).tasks(new URLSearchParams({ board: "factory" }));
const none: TaskFilters = { q: "", priority: "", owner: "", readyOnly: false };
const ids = (filters: Partial<TaskFilters> = {}) =>
  Object.fromEntries(groupTasks(tasks, { ...none, ...filters }, now).map((group) => [group.key, group.tasks.map((task) => task.id)]));

describe("groupTasks", () => {
  test("groups by status, ready first among open, done of the last 30 days only, no dropped", () => {
    expect(ids()).toEqual({
      open: ["factory-15", "factory-16", "factory-18", "factory-20", "factory-21", "factory-19"],
      doing: ["factory-12", "factory-14"],
      blocked: ["factory-17"],
      review: ["factory-13"],
      done: ["factory-9", "factory-8"],
    });
    expect(tasks.find((task) => task.id === "factory-19")?.ready).toBe(false);
  });

  test("filters by priority, owner, readiness and text", () => {
    expect(ids({ priority: "p1" }).open).toEqual(["factory-15"]);
    expect(ids({ owner: "none" }).doing).toEqual([]);
    expect(ids({ readyOnly: true }).open).not.toContain("factory-19");
    expect(ids({ q: "LEASES" }).open).toEqual(["factory-15"]);
    expect(ids({ q: "factory-13" }).review).toEqual(["factory-13"]);
  });
});
