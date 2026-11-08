import { describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen } from "@testing-library/react";
import { renderInApp } from "../test/render.tsx";
import { DevState } from "../dev/state.ts";
import { queries } from "./live/query.ts";
import { BoardLinks, Nav } from "./app.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);

function seed(client: QueryClient) {
  client.setQueryData(
    queries.openAsks().queryKey,
    state.asks(new URLSearchParams({ status: "open" })),
  );
  client.setQueryData(queries.boards().queryKey, state.boards());
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

  test("Needs you counts the open asks in filled amber; Home has no count", async () => {
    await renderInApp(<Nav />, { seed });
    const open = state.asks(new URLSearchParams({ status: "open" })).length;
    const asks = screen.getByLabelText(`${open} waiting on you`);
    expect(asks.textContent).toBe(String(open));
    expect(asks.className).toMatch(/(^| )bg-amber( |$)/);
    const home = screen.getByRole("link", { name: /Home/ });
    expect(home.textContent).toBe("Home");
    expect(screen.getByRole("link", { name: /Needs you/ }).textContent).not.toMatch(/unseen/);
  });

  test("there is no Events item", async () => {
    await renderInApp(<Nav />, { seed });
    expect(screen.queryByRole("link", { name: /Events/ })).toBeNull();
  });
});

describe("board links", () => {
  // A board named `web` is a prefix of `web-view`: a task of web-view must not mark it.
  function seedWithPrefix(client: QueryClient) {
    const web = { ...state.board("web-view"), slug: "web", title: "Web" };
    client.setQueryData(queries.boards().queryKey, [web, ...state.boards()]);
  }

  test.each([
    ["/tasks/web-view-3", "web-view"],
    ["/tasks/web-3", "web"],
    ["/boards/web", "web"],
  ])("on %s only the %s board is current", async (path, slug) => {
    await renderInApp(<BoardLinks />, { path, seed: seedWithPrefix });
    const current = screen
      .getAllByRole("link")
      .filter((link) => link.getAttribute("aria-current") === "page")
      .map((link) => link.getAttribute("href"));
    expect(current).toEqual([`/boards/${slug}`]);
  });
});
