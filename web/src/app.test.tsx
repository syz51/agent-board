import { describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen } from "@testing-library/react";
import { renderInApp } from "../test/render.tsx";
import { DevState } from "../dev/state.ts";
import { queries } from "./live/query.ts";
import { Nav } from "./app.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();

function seed(client: QueryClient) {
  client.setQueryData(
    queries.openAsks().queryKey,
    state.asks(new URLSearchParams({ status: "open" })),
  );
  client.setQueryData(queries.candidateTasks().queryKey, []);
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(queries.updates("").queryKey, state.updates(none));
}

const current = () =>
  screen
    .getAllByRole("link")
    .filter((link) => link.getAttribute("aria-current") === "page")
    .map((link) => link.textContent);

describe("navigation", () => {
  test.each([
    ["/", "Boards"],
    ["/boards/factory?ready=1", "Boards"],
    ["/tasks/factory-9", "Boards"],
    ["/asks/3", "Needs you"],
    ["/agents?state=running", "Agents"],
    ["/events", "Events"],
  ])("on %s only %s is current", async (path, label) => {
    await renderInApp(<Nav />, { path, seed });
    const links = current();
    expect(links.length).toBe(1);
    expect(links[0]).toStartWith(label);
  });

  test("Needs you names its count and unseen updates for screen readers", async () => {
    await renderInApp(<Nav />, { seed });
    const open = state.asks(new URLSearchParams({ status: "open" })).length;
    expect(screen.getByLabelText(`${open} waiting on you`).textContent).toBe(String(open));
    expect(screen.getByRole("link", { name: /Needs you/ }).textContent).toMatch(/unseen updates?$/);
  });
});
