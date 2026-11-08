import { afterEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, waitFor } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();

const title = state.boards().find((board) => board.slug === "factory")!.title;

const original = globalThis.fetch;
/** The query of each task list request, in order. */
let asked: URLSearchParams[] = [];
afterEach(() => {
  globalThis.fetch = original;
  asked = [];
  localStorage.clear();
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
    if (url.pathname === "/v1/tasks") {
      asked.push(url.searchParams);
      return Response.json(state.tasks(url.searchParams));
    }
    return Response.json(
      { code: "not_found", message: `No fixture for ${url.pathname}.`, next: null },
      { status: 404 },
    );
  }) as typeof fetch;
}

function seed(client: QueryClient) {
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(queries.openAsks().queryKey, []);
  client.setQueryData(queries.agents().queryKey, state.agents(none));
}

const bar = () => screen.queryByRole("search");

/** The factory board while every request fails and nothing but the agents is cached. */
async function outage() {
  serve(true);
  await renderRoute("/boards/factory", {
    seed: (client) => client.setQueryData(queries.agents().queryKey, state.agents(none)),
  });
  await waitFor(() => expect(screen.getByText(/board server is down/)).toBeTruthy(), {
    timeout: 4000,
  });
}

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

  test("without the board list the page names the board by its slug, in the title style", async () => {
    await outage();
    const heading = screen.getByRole("heading", { level: 1 });
    expect(heading.textContent).toBe("factory");
    expect(heading.querySelector(".ident")).toBeNull();
  });

  test("without the board list the page uses the name this browser saw last", async () => {
    serve();
    const { unmount } = await renderRoute("/boards/factory", { seed });
    await waitFor(() => expect(screen.getByRole("heading", { level: 1 }).textContent).toBe(title));
    unmount();
    await outage();
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe(title);
    expect(document.title).toStartWith(title);
  });

  test("stays while filters are set, so they can be cleared", async () => {
    serve();
    await renderRoute("/boards/sandbox?q=nothing", { seed });
    await waitFor(() => expect(screen.getByText("No tasks match these filters")).toBeTruthy());
    expect(bar()).not.toBeNull();
  });
});

describe("a board narrowed to a parent", () => {
  test("names the parent, counts its children by status and lists only them", async () => {
    serve();
    await renderRoute("/boards/factory?parent=factory-12", { seed });
    const banner = await waitFor(() => screen.getByRole("region", { name: /^Subtasks of/ }));
    await waitFor(() => expect(banner.textContent).toContain("Inject board delta"));
    const tally = screen.getByRole("list", { name: "Subtasks by status" });
    expect(
      Array.from(tally.querySelectorAll("li"), (li) => li.textContent?.replace(/\s+/g, " ")),
    ).toEqual(["2 done", "1 review", "1 blocked", "1 ready", "1 waiting"]);
    // The id cell of each row, not the ids its blockers link to.
    const listed = Array.from(document.querySelectorAll("li > .ident"), (el) => el.textContent);
    expect(listed.sort()).toEqual(
      ["factory-13", "factory-17", "factory-19", "factory-21", "factory-3", "factory-8"].sort(),
    );
    // The children come from the server's parent filter, not the capped board list.
    expect(
      asked.some((q) => q.get("parent") === "factory-12" && q.get("board") === "factory"),
    ).toBe(true);
    const clear = screen.getByRole("link", { name: "Show all tasks" });
    expect(clear.getAttribute("href")).toBe("/boards/factory");
    // The filter bar stays, so the narrowing can be combined and cleared.
    expect(bar()).not.toBeNull();
  });
});
