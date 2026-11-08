import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, waitFor, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";
import { agentAnchor } from "../ui/common.tsx";

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
  client.setQueryData(queries.agents().queryKey, agents);
  client.setQueryData(
    queries.doingTasks().queryKey,
    state.tasks(new URLSearchParams({ status: "doing" })),
  );
  for (const agent of agents) {
    client.setQueryData(queries.handoff(agent.id).queryKey, handoff(agent.id));
  }
}

/** An agent's card, found by the anchor other pages link to. */
const card = (handle: string) =>
  waitFor(() => {
    const found = document.getElementById(agentAnchor(handle));
    if (!found) throw new Error(`no card for ${handle}`);
    return found;
  });

describe("the agents list", () => {
  test("names each agent by its thread title beside its handle, and links its T3 thread", async () => {
    await renderRoute("/agents", { seed });
    const linked = agents.find((agent) => agent.title && agent.native_ids.t3_link)!;
    const row = await card(linked.handle);
    const heading = within(row).getByRole("heading", { level: 2 });
    expect(heading.textContent).toBe(`${linked.title}${linked.handle}`);
    const open = within(row).getByRole("link", { name: /Open in T3 Code/ });
    expect(open.getAttribute("href")).toBe(linked.native_ids.t3_link ?? "");
    expect(open.getAttribute("target")).toBe("_blank");
    expect(open.getAttribute("rel")).toBe("noopener noreferrer");
    // An agent without a thread link has no such link.
    const unlinked = agents.find((agent) => agent.state !== "ended" && !agent.native_ids.t3_link)!;
    const other = await card(unlinked.handle);
    expect(within(other).queryByRole("link", { name: /Open in T3 Code/ })).toBeNull();
  });

  test("says where each agent runs in one line under its name, with no details column", async () => {
    await renderRoute("/agents", { seed });
    const coordinator = agents.find((agent) => agent.role === "coordinator" && agent.repo)!;
    const row = await card(coordinator.handle);
    const line = row.querySelector("header [data-slot=where]");
    expect(line?.textContent).toBe(
      [
        "Coordinator",
        `${coordinator.device} · ${coordinator.runtime}`,
        coordinator.repo!.replace(/^github\.com\//, ""),
        ...(coordinator.branch ? [coordinator.branch] : []),
      ].join(" · "),
    );
    expect(within(row).queryByText("Role")).toBeNull();
    expect(screen.queryByText("Details")).toBeNull();
  });

  test("tags in a handoff show as inert code", async () => {
    await renderRoute("/agents", { seed });
    const marked = agents.find((agent) => /<\/?[a-z]/i.test(handoff(agent.id)?.now ?? ""))!;
    expect(marked).toBeTruthy();
    const row = await card(marked.handle);
    const tags = [...row.querySelectorAll("code.raw-tag")].map((node) => node.textContent);
    expect(tags).toContain("</board>");
  });
});
