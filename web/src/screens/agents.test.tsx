import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, waitFor, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();
const agents = state.agents(none);

/** An agent's handoff, or null where it has written none, as the API answers. */
function handoff(id: string) {
  try {
    return state.handoff(id);
  } catch {
    return null;
  }
}

// Coordinators' turn endings are not seeded; they load as an empty list.
const original = globalThis.fetch;
beforeEach(() => {
  globalThis.fetch = (async () => Response.json([])) as unknown as typeof fetch;
});
afterEach(() => {
  globalThis.fetch = original;
});

function seed(client: QueryClient) {
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(queries.openAsks().queryKey, []);
  client.setQueryData(queries.updates("").queryKey, state.updates(none));
  client.setQueryData(queries.agents().queryKey, agents);
  client.setQueryData(
    queries.doingTasks().queryKey,
    state.tasks(new URLSearchParams({ status: "doing" })),
  );
  for (const agent of agents) {
    client.setQueryData(queries.handoff(agent.id).queryKey, handoff(agent.id));
  }
}

describe("the agents list", () => {
  test("says where each agent runs under its name, its machine then its code, with no details column", async () => {
    await renderRoute("/agents", { seed });
    const coordinator = agents.find((agent) => agent.role === "coordinator" && agent.repo)!;
    const row = await waitFor(() =>
      screen.getByRole("heading", { level: 2, name: coordinator.handle }).closest("article")!,
    );
    const where = row.querySelector("header [data-slot=where]");
    expect(where?.textContent).toBe(`Coordinator · ${coordinator.device} · ${coordinator.runtime}`);
    const code = row.querySelector("header [data-slot=code]");
    expect(code?.textContent).toBe(
      [
        coordinator.repo!.replace(/^github\.com\//, ""),
        ...(coordinator.branch ? [coordinator.branch] : []),
      ].join(" · "),
    );
    expect(code?.className).toContain("font-mono");
    expect(within(row).queryByText("Role")).toBeNull();
    expect(screen.queryByText("Details")).toBeNull();
  });

  test("tags in a handoff show as inert code", async () => {
    await renderRoute("/agents", { seed });
    const marked = agents.find((agent) => /<\/?[a-z]/i.test(handoff(agent.id)?.now ?? ""))!;
    expect(marked).toBeTruthy();
    const row = await waitFor(() =>
      screen.getByRole("heading", { level: 2, name: marked.handle }).closest("article")!,
    );
    const tags = [...row.querySelectorAll("code.raw-tag")].map((node) => node.textContent);
    expect(tags).toContain("</board>");
  });
});
