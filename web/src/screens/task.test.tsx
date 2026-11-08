import { describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, waitFor, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();

function seed(id: string) {
  return (client: QueryClient) => {
    const detail = state.task(id);
    client.setQueryData(queries.boards().queryKey, state.boards());
    client.setQueryData(queries.openAsks().queryKey, []);
    client.setQueryData(queries.agents().queryKey, state.agents(none));
    client.setQueryData(queries.task(id).queryKey, detail);
    client.setQueryData(
      queries.boardTasks(detail.task.board).queryKey,
      state.tasks(new URLSearchParams({ board: detail.task.board })),
    );
  };
}

describe("a task's page", () => {
  test("says near the top what blocks the task and how many subtasks it has", async () => {
    await renderRoute("/tasks/factory-12", { seed: seed("factory-12") });
    const blockers = await waitFor(() =>
      screen.getByRole("link", { name: /^Blocked by factory-9$/ }),
    );
    const subtasks = screen.getByRole("link", { name: "6 subtasks" });
    // Each points at its full list lower on the page.
    for (const link of [blockers, subtasks]) {
      const target = document.getElementById(link.getAttribute("href")!.slice(1));
      expect(target?.closest("section")).not.toBeNull();
    }
    expect(document.getElementById("task-blocked-by")?.textContent).toContain("factory-9");
  });

  test("has no such line when the task links no other tasks", async () => {
    await renderRoute("/tasks/factory-9", { seed: seed("factory-9") });
    await waitFor(() => screen.getByRole("heading", { level: 1 }));
    expect(screen.queryByRole("link", { name: /^Blocked by/ })).toBeNull();
    expect(screen.queryByRole("link", { name: /subtasks?$/ })).toBeNull();
  });

  test("counts the subtasks by status and lists each with its status", async () => {
    await renderRoute("/tasks/factory-12", { seed: seed("factory-12") });
    const section = await waitFor(() => screen.getByRole("region", { name: /^Subtasks/ }));
    const tally = within(section).getByRole("list", { name: "Subtasks by status" });
    expect(
      Array.from(tally.querySelectorAll("li"), (li) => li.textContent?.replace(/\s+/g, " ")),
    ).toEqual(["2 done", "1 review", "1 blocked", "1 ready", "1 waiting"]);
    const rows = Array.from(section.querySelectorAll("[data-child]"), (row) => [
      row.getAttribute("data-child"),
      row.querySelector("[role=img]")?.getAttribute("aria-label"),
    ]);
    expect(rows).toEqual([
      ["factory-3", "Done"],
      ["factory-8", "Done"],
      ["factory-13", "Review"],
      ["factory-17", "Blocked"],
      ["factory-19", "Open"],
      ["factory-21", "Ready"],
    ]);
    expect(
      within(section)
        .getByRole("link", { name: /Open as a list/ })
        .getAttribute("href"),
    ).toBe("/boards/factory?parent=factory-12");
  });

  test("shows each subtask's owner from the detail alone", async () => {
    // No board list: it never arrives, and the detail itself says who works on each child.
    const original = globalThis.fetch;
    globalThis.fetch = (() => new Promise(() => {})) as unknown as typeof fetch;
    await renderRoute("/tasks/factory-12", {
      seed: (client) => {
        client.setQueryData(queries.boards().queryKey, state.boards());
        client.setQueryData(queries.openAsks().queryKey, []);
        client.setQueryData(queries.agents().queryKey, state.agents(none));
        client.setQueryData(queries.task("factory-12").queryKey, state.task("factory-12"));
      },
    });
    const section = await waitFor(() => screen.getByRole("region", { name: /^Subtasks/ }));
    const row = (id: string) => section.querySelector(`[data-child="${id}"]`)?.textContent;
    expect(row("factory-13")).toContain("t3:a91c");
    expect(row("factory-21")).toContain("Unowned");
    globalThis.fetch = original;
  });

  test("a subtask links its parent above its title", async () => {
    await renderRoute("/tasks/factory-21", { seed: seed("factory-21") });
    const link = await waitFor(() =>
      screen.getByRole("link", { name: /factory-12 Inject board delta/ }),
    );
    expect(link.getAttribute("href")).toBe("/tasks/factory-12");
    expect(link.closest("header")).not.toBeNull();
  });

  test("a task without children has no subtasks section", async () => {
    await renderRoute("/tasks/factory-9", { seed: seed("factory-9") });
    await waitFor(() => screen.getByRole("heading", { level: 1 }));
    expect(screen.queryByRole("region", { name: /^Subtasks/ })).toBeNull();
    expect(screen.queryByText("Subtask of")).toBeNull();
  });

  test.each([
    // A feedback task: Summary follows the Feedback section, so it keeps the section gap.
    ["docs-3", "mt-7"],
    // No feedback: Summary opens the main column with no gap.
    ["factory-9", ""],
  ])("on %s the Summary section has the class %p", async (id, className) => {
    await renderRoute(`/tasks/${id}`, { seed: seed(id) });
    const summary = await waitFor(() => screen.getByRole("region", { name: "Summary" }));
    expect(summary.className).toBe(className);
    expect(screen.queryByRole("region", { name: /^Feedback/ }) !== null).toBe(id === "docs-3");
  });
});
