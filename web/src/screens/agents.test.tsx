import { afterEach, beforeEach, describe, expect, setSystemTime, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, waitFor, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";
import { agentAnchor } from "../ui/common.tsx";
import { isAway, sortAgents } from "./agents.tsx";
import type { Agent } from "../api/schema.ts";

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

// Coordinators' turn endings are not seeded; they load as an empty list. The page's clock is
// the fixtures' one, so agents count as away from when the fixtures last saw them.
const original = globalThis.fetch;
beforeEach(() => {
  globalThis.fetch = (async () => Response.json([])) as unknown as typeof fetch;
  setSystemTime(new Date(now));
});
afterEach(() => {
  globalThis.fetch = original;
  setSystemTime();
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

  test("highlights the card another page linked to", async () => {
    const [linked, other] = agents.filter((agent) => agent.state !== "ended");
    await renderRoute(`/agents#${agentAnchor(linked!.handle)}`, { seed });
    expect((await card(linked!.handle)).className).toContain("bg-accent");
    expect((await card(other!.handle)).className).not.toContain("bg-accent");
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

  // An idle agent the fixtures last saw 31 hours ago, and one seen minutes ago.
  const away = agents.find((agent) => agent.handle === "t3:e4b7")!;
  const live = agents.find((agent) => agent.handle === "t3:a91c")!;

  test("leaves agents unseen for half an hour out of Active, and counts them", async () => {
    expect(away.state).toBe("idle");
    await renderRoute("/agents", { seed });
    await card(live.handle);
    expect(document.getElementById(agentAnchor(away.handle))).toBeNull();
    expect(screen.getByText(/· 1 away/)).toBeTruthy();
  });

  test("lists agents unseen for half an hour under Away", async () => {
    await renderRoute("/agents?state=away", { seed });
    await card(away.handle);
    expect(document.getElementById(agentAnchor(live.handle))).toBeNull();
  });
});

describe("away agents", () => {
  const minute = 60_000;
  const at = (minutesAgo: number) => new Date(now - minutesAgo * minute).toISOString();
  const base = agents.find((agent) => agent.role === "worker" && agent.state !== "ended")!;
  const make = (fields: Partial<Agent>): Agent => ({ ...base, paused_until: null, ...fields });
  const noCoordinators = new Set<string>();

  test("an agent that has not ended is away once unseen for more than 30 minutes", () => {
    for (const state of ["running", "idle"] as const) {
      expect(isAway(make({ state, last_seen: at(30) }), now, noCoordinators)).toBe(false);
      expect(isAway(make({ state, last_seen: at(31) }), now, noCoordinators)).toBe(true);
    }
    expect(isAway(make({ state: "ended", last_seen: at(3 * 24 * 60) }), now, noCoordinators)).toBe(
      false,
    );
  });

  test("agents that wait for a reset ahead, the user, or hold a board stay listed", () => {
    const old = at(48 * 60);
    expect(
      isAway(make({ state: "paused", paused_until: at(-60), last_seen: old }), now, noCoordinators),
    ).toBe(false);
    expect(isAway(make({ state: "paused", last_seen: old }), now, noCoordinators)).toBe(false);
    // A reset that has passed no longer holds it.
    expect(
      isAway(make({ state: "paused", paused_until: at(5), last_seen: old }), now, noCoordinators),
    ).toBe(true);
    const coordinator = make({ state: "idle", last_seen: old });
    expect(isAway(coordinator, now, new Set([coordinator.id]))).toBe(false);
  });

  test("each filter takes its agents", () => {
    const list = [
      make({ id: "a", state: "running", last_seen: at(1) }),
      make({ id: "b", state: "running", last_seen: at(120) }),
      make({ id: "c", state: "idle", last_seen: at(2) }),
      make({ id: "d", state: "ended", last_seen: at(3) }),
    ];
    const ids = (filter: string) =>
      sortAgents(list, filter, now, noCoordinators).map((agent) => agent.id);
    expect(ids("")).toEqual(["a", "c"]);
    expect(ids("running")).toEqual(["a"]);
    expect(ids("away")).toEqual(["b"]);
    expect(ids("ended")).toEqual(["d"]);
    expect(ids("all")).toEqual(["a", "b", "c", "d"]);
  });
});
