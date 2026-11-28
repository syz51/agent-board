import { describe, expect, test } from "vite-plus/test";
import { screen, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderInApp } from "../../test/render.tsx";
import type { Task } from "../api/schema.ts";
import { agentIndex, formatAbsolute, formatDay } from "../ui/common.tsx";
import { TaskList } from "./board.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const agents = agentIndex(state.agents(new URLSearchParams()));
const base = state.tasks(new URLSearchParams())[0]!;

const row = async (task: Task, done: ReadonlySet<string> = new Set()) => {
  await renderInApp(<TaskList tasks={[task]} agents={agents} done={done} now={now} />);
  return screen.getByRole("listitem");
};

describe("task rows", () => {
  test("open blockers read as muted text with links, not as a tag", async () => {
    const item = await row({ ...base, status: "open", blockers: ["factory-14", "factory-2"] });
    const blockers = item.querySelector("[data-dependency=blockers]");
    expect(blockers?.textContent).toBe("blocked by factory-14, factory-2");
    expect(blockers?.closest("[data-slot=badge]")).toBeNull();
    expect(within(item).getByRole("link", { name: "factory-14" }).getAttribute("href")).toBe(
      "/tasks/factory-14",
    );
  });

  test("a blocker already done drops out", async () => {
    const item = await row({ ...base, blockers: ["factory-14"] }, new Set(["factory-14"]));
    expect(item.querySelector("[data-dependency=blockers]")).toBeNull();
  });

  test("a task held only by closed blockers says so", async () => {
    const held = { ...base, status: "blocked" as const, blockers: ["factory-14"] };
    const item = await row({ ...held, blockers_closed: true }, new Set(["factory-14"]));
    expect(item.querySelector("[data-dependency=closed]")?.textContent).toBe(
      "every blocker is done or dropped",
    );
  });

  test("a task that still waits on a blocker in progress does not", async () => {
    const item = await row({ ...base, status: "blocked", blockers: ["factory-14"] });
    expect(item.querySelector("[data-dependency=closed]")).toBeNull();
  });

  test("a start date ahead shows as a short day, with the full time on hover", async () => {
    const at = "2026-11-22T11:47:00Z";
    const item = await row({ ...base, status: "open", not_before: at });
    const waits = item.querySelector("[data-dependency=date]");
    expect(waits?.textContent).toBe(`waits until ${formatDay(at, now)}`);
    expect(waits?.closest("[data-slot=badge]")).toBeNull();
    expect(waits?.querySelector("time")?.getAttribute("title")).toBe(formatAbsolute(at));
  });

  test("a start date already past shows nothing", async () => {
    const item = await row({ ...base, status: "open", not_before: "2026-11-01T00:00:00Z" });
    expect(item.querySelector("[data-dependency=date]")).toBeNull();
  });
});

describe("formatDay", () => {
  test("drops the year only within this year", () => {
    expect(formatDay("2026-11-22T11:47:00Z", now)).not.toMatch(/2026/);
    expect(formatDay("2027-01-03T12:00:00Z", now)).toMatch(/2027/);
    expect(formatDay("2026-11-22T11:47:00Z", now)).not.toMatch(/:/);
  });
});

describe("an umbrella task's row", () => {
  test("links its child count to the list narrowed to its children", async () => {
    const item = await row({ ...base, id: "factory-12", board: "factory", child_count: 6 });
    const children = item.querySelector("[data-dependency=children]");
    expect(children?.textContent).toBe("6 subtasks");
    expect(children?.getAttribute("href")).toBe("/boards/factory?parent=factory-12");
  });

  test("a task without children has no such link", async () => {
    const item = await row({ ...base, child_count: 0 });
    expect(item.querySelector("[data-dependency=children]")).toBeNull();
  });

  test("shows an Expedited badge when task.expedited is true", async () => {
    const expeditedItem = await row({ ...base, expedited: true });
    expect(expeditedItem.querySelector("[data-badge=expedited]")?.textContent).toBe("Expedited");
  });

  test("omits the Expedited badge when task.expedited is false", async () => {
    const normalItem = await row({ ...base, id: "factory-99", expedited: false });
    expect(normalItem.querySelector("[data-badge=expedited]")).toBeNull();
  });
});
