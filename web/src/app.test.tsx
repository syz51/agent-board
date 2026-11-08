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
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(queries.updates("").queryKey, state.updates(none));
}

const marked = (value: string) =>
  screen
    .getAllByRole("link")
    .filter((link) => link.getAttribute("aria-current") === value)
    .map((link) => link.textContent);

describe("navigation", () => {
  test.each([
    ["/", "Home"],
    ["/asks/3", "Needs you"],
    ["/agents?state=running", "Agents"],
  ])("on %s only %s is the current page", async (path, label) => {
    await renderInApp(<Nav />, { path, seed });
    const links = marked("page");
    expect(links.length).toBe(1);
    expect(links[0]).toStartWith(label);
    expect(marked("true")).toEqual([]);
  });

  test.each(["/boards/factory?ready=1", "/tasks/factory-9"])(
    "on %s Home holds the page but is not it, so only the board is shown current",
    async (path) => {
      await renderInApp(<Nav />, { path, seed });
      expect(marked("page")).toEqual([]);
      const holders = marked("true");
      expect(holders.length).toBe(1);
      expect(holders[0]).toStartWith("Home");
    },
  );

  test("Needs you counts the open asks; Home names its unseen updates", async () => {
    await renderInApp(<Nav />, { seed });
    const open = state.asks(new URLSearchParams({ status: "open" })).length;
    expect(screen.getByLabelText(`${open} waiting on you`).textContent).toBe(String(open));
    const home = screen.getByRole("link", { name: /Home/ });
    expect(home.textContent).toMatch(/unseen updates?$/);
    // The same count as the home page's "Since you last looked", saying what it counts.
    const unseen = state.updates(none);
    const total = unseen.done.length + unseen.said.length;
    expect(total).toBeGreaterThan(0);
    const count = home.querySelector("[title]");
    expect(count?.getAttribute("title")).toBe(`${total} new since you last looked`);
    expect(count?.textContent).toBe(`${total} new`);
    expect(screen.getByRole("link", { name: /Needs you/ }).textContent).not.toMatch(/unseen/);
  });

  test("there is no Events item", async () => {
    await renderInApp(<Nav />, { seed });
    expect(screen.queryByRole("link", { name: /Events/ })).toBeNull();
  });
});
