import { describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, waitFor } from "@testing-library/react";
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
    client.setQueryData(queries.updates("").queryKey, state.updates(none));
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
    const subtasks = screen.getByRole("link", { name: "1 subtask" });
    // Each points at its full list lower on the page.
    for (const link of [blockers, subtasks]) {
      const target = document.getElementById(link.getAttribute("href")!.slice(1));
      expect(target?.tagName).toBe("SECTION");
    }
    expect(document.getElementById("task-blocked-by")?.textContent).toContain("factory-9");
  });

  test("has no such line when the task links no other tasks", async () => {
    await renderRoute("/tasks/factory-9", { seed: seed("factory-9") });
    await waitFor(() => screen.getByRole("heading", { level: 1 }));
    expect(screen.queryByRole("link", { name: /^Blocked by/ })).toBeNull();
    expect(screen.queryByRole("link", { name: /subtasks?$/ })).toBeNull();
  });
});
