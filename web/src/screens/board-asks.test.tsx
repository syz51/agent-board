import { afterEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, waitFor, within } from "@testing-library/react";
import type { Ask } from "../api/schema.ts";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";
import { BOARD_ASK_ROWS } from "./board.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();
const open = state.asks(new URLSearchParams({ status: "open" }));

const original = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = original;
  localStorage.clear();
});

/** Board tasks from the fixtures; the shell's data, with `asks` as the open asks, is seeded. */
function show(asks: Ask[]) {
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = new URL(String(input), "http://localhost");
    return Response.json(state.tasks(url.searchParams));
  }) as typeof fetch;
  return (client: QueryClient) => {
    client.setQueryData(queries.boards().queryKey, state.boards());
    client.setQueryData(queries.openAsks().queryKey, asks);
    client.setQueryData(queries.agents().queryKey, state.agents(none));
  };
}

const region = () => screen.queryByRole("region", { name: /Waiting on you/ });

describe("a board's open asks", () => {
  test("list this board's open asks above its tasks, newest first, each opening its ask", async () => {
    const board = open[0]!.board;
    const mine = open.filter((ask) => ask.board === board);
    await renderRoute(`/boards/${board}`, { seed: show(open) });
    await waitFor(() => expect(region()).not.toBeNull());
    const rows = within(region()!).getAllByRole("link");
    const newest = [...mine]
      .sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at) || b.id - a.id)
      .slice(0, BOARD_ASK_ROWS);
    expect(rows.map((row) => row.getAttribute("href"))).toEqual(
      newest.map((ask) => `/asks/${ask.id}`),
    );
    // Above the task groups.
    const [firstGroup] = await screen.findAllByRole("heading", { level: 2, name: /Doing|Ready/ });
    expect(
      region()!.compareDocumentPosition(firstGroup!) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
  });

  test("link to all of them on Needs you when there are more than the page lists", async () => {
    const board = open[0]!.board;
    const many = Array.from({ length: BOARD_ASK_ROWS + 2 }, (_, index) => ({
      ...open[0]!,
      id: 1000 + index,
      board,
    }));
    await renderRoute(`/boards/${board}`, { seed: show(many) });
    await waitFor(() => expect(region()).not.toBeNull());
    const all = within(region()!).getByRole("link", { name: /All \d+ asks/ });
    expect(all.getAttribute("href")).toBe(`/asks?board=${board}`);
    expect(
      within(region()!)
        .getAllByRole("link")
        .filter((link) => link.closest("li")).length,
    ).toBe(BOARD_ASK_ROWS);
  });

  test("show nothing on a board with no open ask", async () => {
    const board = open[0]!.board;
    await renderRoute(`/boards/${board}`, {
      seed: show(open.filter((ask) => ask.board !== board)),
    });
    await screen.findByRole("search");
    expect(region()).toBeNull();
  });
});
