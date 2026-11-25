import { describe, expect, test } from "vite-plus/test";
import { DevState } from "../../dev/state.ts";
import type { Task } from "../api/schema.ts";
import {
  buildPinTopOrder,
  buildReorderOrder,
  inDispatchOrder,
  inProposalOrder,
  partitionBoardTiers,
  UP_NEXT_FRONTIER_SIZE,
} from "../ui/task-signals.tsx";
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
    expect(typeof notBefore).toBe("string");
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

describe("v2 4-tier partitioning and dispatch ordering", () => {
  const template = tasks.find((task) => task.id === "factory-16")!;
  const makeTask = (overrides: Partial<Task> & { id: string }): Task => ({
    ...template,
    status: "open",
    priority: "p2",
    effective_priority: null,
    rank: null,
    origin: "agent",
    size: null,
    cooled_at: null,
    ready_at: "2026-11-05T00:00:00Z",
    report_count: 0,
    auto: true,
    proposal: false,
    ready: true,
    blockers: [],
    feedback: null,
    ...overrides,
  });

  test("inDispatchOrder respects effective_priority, explicit rank, WSJF tie-break, and number", () => {
    const inherited = makeTask({ id: "factory-40", priority: "p3", effective_priority: "p1" });
    const p1Ranked = makeTask({ id: "factory-41", priority: "p1", rank: 1 });
    const p2SmallHot = makeTask({ id: "factory-42", priority: "p2", size: "xs", report_count: 6 });
    const p2LargeCold = makeTask({ id: "factory-43", priority: "p2", size: "l", report_count: 0 });
    const p2Ranked = makeTask({ id: "factory-44", priority: "p2", rank: 2 });

    const ordered = [p2LargeCold, inherited, p2SmallHot, p2Ranked, p1Ranked].sort((a, b) =>
      inDispatchOrder(a, b),
    );
    expect(ordered.map((t) => t.id)).toEqual([
      "factory-41", // p1 ranked #1
      "factory-40", // effective p1 unranked
      "factory-44", // p2 ranked #2
      "factory-42", // p2 unranked higher WSJF (xs + 6 reports)
      "factory-43", // p2 unranked lower WSJF (l + 0 reports)
    ]);
  });

  test("inProposalOrder sorts by effective priority, report_count DESC, rank, created_at", () => {
    const olderLow = makeTask({
      id: "factory-50",
      priority: "p2",
      report_count: 1,
      created_at: "2026-11-01T00:00:00Z",
      auto: false,
      proposal: true,
    });
    const newerHot = makeTask({
      id: "factory-51",
      priority: "p2",
      report_count: 5,
      created_at: "2026-11-05T00:00:00Z",
      auto: false,
      proposal: true,
    });
    const p1Prop = makeTask({
      id: "factory-52",
      priority: "p1",
      report_count: 0,
      auto: false,
      proposal: true,
    });

    const ordered = [olderLow, newerHot, p1Prop].sort(inProposalOrder);
    expect(ordered.map((t) => t.id)).toEqual(["factory-52", "factory-51", "factory-50"]);
  });

  test("partitionBoardTiers splits Top-20 Up Next Frontier, Proposals, Backlog, Waiting, and Icebox", () => {
    const manyReady = Array.from({ length: 24 }, (_, idx) =>
      makeTask({
        id: `factory-${100 + idx}`,
        priority: idx < 5 ? "p1" : "p2",
        rank: idx < 3 ? idx + 1 : null,
      }),
    );
    const proposal = makeTask({
      id: "factory-200",
      auto: false,
      proposal: true,
      ready: false,
      body: "Proposal: add feature",
    });
    const waiting = makeTask({
      id: "factory-201",
      auto: true,
      ready: false,
      blockers: ["factory-100"],
    });
    const cooled = makeTask({
      id: "factory-202",
      priority: "p3",
      cooled_at: "2026-11-01T00:00:00Z",
      ready: false,
    });

    const all = [...manyReady, proposal, waiting, cooled];
    const tiers = partitionBoardTiers(all, none, now);
    expect(tiers.upNext.length).toBe(UP_NEXT_FRONTIER_SIZE);
    expect(tiers.backlogReady.length).toBe(4);
    expect(tiers.totalReadyAuto).toBe(24);
    expect(tiers.proposals.map((t) => t.id)).toEqual(["factory-200"]);
    expect(tiers.waiting.map((t) => t.id)).toEqual(["factory-201"]);
    expect(tiers.iceboxInline.map((t) => t.id)).toEqual(["factory-202"]);

    // Filtering by p2 preserves the global Top-20 frontier boundary (15 p2 in Up Next, 4 in Backlog).
    const p2Only = partitionBoardTiers(all, { ...none, priority: "p2" }, now);
    expect(p2Only.upNext.length).toBe(15);
    expect(p2Only.backlogReady.length).toBe(4);
  });

  test("buildPinTopOrder and buildReorderOrder maintain clean per-level rank sequences", () => {
    const a = makeTask({ id: "factory-1", priority: "p2", rank: 1 });
    const b = makeTask({ id: "factory-2", priority: "p2", rank: 2 });
    const c = makeTask({ id: "factory-3", priority: "p2", rank: null });
    const p1 = makeTask({ id: "factory-4", priority: "p1", rank: 1 });
    const all = [p1, a, b, c];

    expect(buildPinTopOrder(c, all)).toEqual(["factory-3", "factory-1", "factory-2"]);
    expect(buildReorderOrder("factory-3", "factory-2", "before", all, all)).toEqual([
      "factory-1",
      "factory-3",
      "factory-2",
    ]);
    // Cross-priority drags are rejected cleanly.
    expect(buildReorderOrder("factory-3", "factory-4", "before", all, all)).toBeNull();
    // Moving an already-ranked task into its own position is a no-op.
    expect(buildReorderOrder("factory-1", "factory-2", "before", all, all)).toBeNull();
  });

  test("accepted proposals (auto=true, proposal=false) move to Up Next even if body starts with Proposal:", () => {
    const accepted = makeTask({
      id: "factory-300",
      auto: true,
      proposal: false,
      ready: true,
      body: "Proposal: already accepted by verdict judge",
    });
    const tiers = partitionBoardTiers([accepted], none, now);
    expect(tiers.upNext.map((t) => t.id)).toEqual(["factory-300"]);
    expect(tiers.proposals).toEqual([]);
  });

  test("buildReorderOrder preserves hidden higher-ranked tasks and rejects cross-band inherited reorders", () => {
    const ranked11 = Array.from({ length: 11 }, (_, idx) =>
      makeTask({ id: `factory-${600 + idx}`, priority: "p2", rank: idx + 1 }),
    );
    // Filtered view shows only #10 and #11; moving #11 before #10 must keep #1..#9 intact.
    const visibleFiltered = [ranked11[9]!, ranked11[10]!];
    const order = buildReorderOrder(
      "factory-610",
      "factory-609",
      "before",
      visibleFiltered,
      ranked11,
    );
    expect(order).toEqual([...ranked11.slice(0, 9).map((t) => t.id), "factory-610", "factory-609"]);

    // A p3 task inheriting effective_priority=p1 sits in a different visual band from plain p3.
    const inheritedP3 = makeTask({
      id: "factory-700",
      priority: "p3",
      effective_priority: "p1",
      rank: 1,
    });
    const plainP3 = makeTask({ id: "factory-701", priority: "p3", rank: 2 });
    expect(
      buildReorderOrder(
        "factory-701",
        "factory-700",
        "before",
        [inheritedP3, plainP3],
        [inheritedP3, plainP3],
      ),
    ).toBeNull();
  });
});
