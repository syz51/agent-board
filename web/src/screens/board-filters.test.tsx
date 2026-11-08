import { afterEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, waitFor } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();

const original = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = original;
});

/** Board tasks come from the fixtures, or fail when `down`; the shell's data is seeded. */
function serve(down = false) {
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = new URL(String(input), "http://localhost");
    if (down) {
      return Response.json(
        { code: "unavailable", message: "The board server is down.", next: "Try again." },
        { status: 503 },
      );
    }
    if (url.pathname === "/v1/tasks") return Response.json(state.tasks(url.searchParams));
    return Response.json(
      { code: "not_found", message: `No fixture for ${url.pathname}.`, next: null },
      { status: 404 },
    );
  }) as typeof fetch;
}

function seed(client: QueryClient) {
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(queries.openAsks().queryKey, []);
  client.setQueryData(queries.updates("").queryKey, state.updates(none));
  client.setQueryData(queries.agents().queryKey, state.agents(none));
}

const bar = () => screen.queryByRole("search");

describe("a board's filter bar", () => {
  test("shows on a board with tasks", async () => {
    serve();
    await renderRoute("/boards/factory", { seed });
    await waitFor(() => expect(bar()).not.toBeNull());
    expect(screen.getByRole("searchbox", { name: "Search tasks" })).toHaveProperty(
      "disabled",
      false,
    );
  });

  test("is gone on a board with no tasks, which says how tasks arrive", async () => {
    serve();
    await renderRoute("/boards/sandbox", { seed });
    await waitFor(() => expect(screen.getByText("No tasks on this board yet")).toBeTruthy());
    expect(bar()).toBeNull();
  });

  test("is gone when the tasks fail to load", async () => {
    serve(true);
    await renderRoute("/boards/factory", { seed });
    await waitFor(() => expect(screen.getByText(/board server is down/)).toBeTruthy(), {
      timeout: 4000,
    });
    expect(bar()).toBeNull();
  });

  test("stays while filters are set, so they can be cleared", async () => {
    serve();
    await renderRoute("/boards/sandbox?q=nothing", { seed });
    await waitFor(() => expect(screen.getByText("No tasks match these filters")).toBeTruthy());
    expect(bar()).not.toBeNull();
  });
});
