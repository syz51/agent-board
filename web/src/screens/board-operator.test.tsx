import { afterEach, describe, expect, test } from "vite-plus/test";
import type { QueryClient } from "@tanstack/react-query";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";

const now = Date.parse("2026-11-06T12:00:00Z");
const originalFetch = globalThis.fetch;

interface RecordedCall {
  method: string;
  path: string;
  query: string;
  body: unknown;
}

function setupServer(options: { failPatch?: boolean } = {}) {
  const state = new DevState(() => now);
  const calls: RecordedCall[] = [];

  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const rawUrl =
      typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    const url = new URL(rawUrl, "http://localhost");
    const method = init?.method ?? "GET";
    const parsedBody: unknown =
      typeof init?.body === "string" && init.body ? JSON.parse(init.body) : undefined;
    calls.push({ method, path: url.pathname, query: url.search, body: parsedBody });

    if (method === "GET" && url.pathname === "/v1/tasks") {
      return Response.json(state.tasks(url.searchParams));
    }
    if (method === "GET" && url.pathname.startsWith("/v1/tasks/")) {
      return Response.json(state.task(decodeURIComponent(url.pathname.slice(10))));
    }
    if (method === "PATCH" && url.pathname.startsWith("/v1/tasks/")) {
      if (options.failPatch) {
        return Response.json(
          {
            code: "version_conflict",
            message: "Task changed while you were viewing it.",
            next: "Reload and try again.",
          },
          { status: 409 },
        );
      }
      return Response.json(
        state.updateTask(decodeURIComponent(url.pathname.slice(10)), parsedBody),
      );
    }
    if (method === "POST" && url.pathname.endsWith("/rank")) {
      const slug = url.pathname.slice("/v1/boards/".length, -"/rank".length);
      return Response.json(state.rankTasks(slug, parsedBody));
    }
    if (method === "GET" && url.pathname === "/v1/boards") {
      return Response.json(state.boards());
    }
    return Response.json(
      { code: "not_found", message: `No fixture for ${url.pathname}`, next: null },
      { status: 404 },
    );
  }) as typeof fetch;

  const seed = (client: QueryClient) => {
    client.setQueryData(queries.boards().queryKey, state.boards());
    client.setQueryData(queries.openAsks().queryKey, []);
    client.setQueryData(queries.agents().queryKey, state.agents(new URLSearchParams()));
  };

  return { state, calls, seed };
}

afterEach(() => {
  globalThis.fetch = originalFetch;
  localStorage.clear();
});

describe("Operator 4-tier Board UI", () => {
  test("renders 4 tabs with live counts, signal badges, and lazy Icebox loading with Revive", async () => {
    const { calls, seed } = setupServer();
    await renderRoute("/boards/factory", { seed });

    // 4-tier tab bar appears once tasks load.
    const tablist = await waitFor(() => screen.getByRole("tablist", { name: "Board tiers" }));
    const upNextTab = within(tablist).getByRole("tab", { name: /Up Next/ });
    const proposalsTab = within(tablist).getByRole("tab", { name: /Proposals/ });
    const backlogTab = within(tablist).getByRole("tab", { name: /Backlog/ });
    const iceboxTab = within(tablist).getByRole("tab", { name: /Icebox/ });

    expect(upNextTab.getAttribute("aria-selected")).toBe("true");
    // Icebox endpoint is NOT called until the Icebox tab is opened.
    expect(calls.some((c) => c.query.includes("icebox=true"))).toBe(false);

    // Signal badges on factory-15 (#1, User, S) and factory-16 (M, 3×).
    const upNextRegion = screen.getByRole("region", { name: /Up Next/ });
    expect(within(upNextRegion).getByText("#1")).toBeTruthy();
    expect(within(upNextRegion).getByText("User")).toBeTruthy();
    expect(within(upNextRegion).getByText("S")).toBeTruthy();
    expect(within(upNextRegion).getByText("3×")).toBeTruthy();

    // Switch to Proposals tab -> sees manual task factory-18.
    fireEvent.click(proposalsTab);
    await waitFor(() => expect(proposalsTab.getAttribute("aria-selected")).toBe("true"));
    expect(screen.getByText("Mark agents idle on Stop")).toBeTruthy();

    // Switch to Backlog tab -> sees Waiting tasks factory-19 and factory-30.
    fireEvent.click(backlogTab);
    await waitFor(() => expect(backlogTab.getAttribute("aria-selected")).toBe("true"));
    expect(screen.getByText("Show handoff age in the snapshot")).toBeTruthy();
    expect(screen.getByText("Rotate the release signing key")).toBeTruthy();

    // Switch to Icebox tab -> triggers lazy GET /v1/tasks?board=factory&icebox=true.
    fireEvent.click(iceboxTab);
    await waitFor(() =>
      expect(screen.getByText("Archive cold event logs to cold object storage")).toBeTruthy(),
    );
    expect(calls.some((c) => c.query.includes("icebox=true"))).toBe(true);

    // 1-click Revive sends PATCH /v1/tasks/factory-31 with cooled: false.
    fireEvent.click(screen.getByRole("button", { name: "Revive factory-31 from Icebox" }));
    await waitFor(() =>
      expect(
        calls.some(
          (c) =>
            c.method === "PATCH" &&
            c.path === "/v1/tasks/factory-31" &&
            (c.body as { cooled?: boolean }).cooled === false,
        ),
      ).toBe(true),
    );
  });

  test("supports 1-click Pin #1, Priority switch, Icebox cool, and inline Drop with reason", async () => {
    const { calls, seed } = setupServer();
    await renderRoute("/boards/factory", { seed });
    await waitFor(() => screen.getByRole("region", { name: /Up Next/ }));

    // 1-click Pin #1 on factory-16 sends POST /v1/boards/factory/rank.
    fireEvent.click(screen.getByRole("button", { name: "Pin factory-16 to #1" }));
    await waitFor(() =>
      expect(
        calls.some(
          (c) =>
            c.method === "POST" &&
            c.path === "/v1/boards/factory/rank" &&
            (c.body as { tasks?: string[] }).tasks?.[0] === "factory-16",
        ),
      ).toBe(true),
    );

    // 1-click Priority change on factory-20 to P1.
    fireEvent.click(screen.getByRole("button", { name: "Set factory-20 priority to P1" }));
    await waitFor(() =>
      expect(
        calls.some(
          (c) =>
            c.method === "PATCH" &&
            c.path === "/v1/tasks/factory-20" &&
            (c.body as { priority?: string }).priority === "p1",
        ),
      ).toBe(true),
    );

    // 1-click Icebox on factory-21.
    fireEvent.click(screen.getByRole("button", { name: "Move factory-21 to Icebox" }));
    await waitFor(() =>
      expect(
        calls.some(
          (c) =>
            c.method === "PATCH" &&
            c.path === "/v1/tasks/factory-21" &&
            (c.body as { cooled?: boolean }).cooled === true,
        ),
      ).toBe(true),
    );

    // Inline Drop prompt requires a reason before submitting.
    fireEvent.click(screen.getByRole("button", { name: "Drop factory-16" }));
    const reasonInput = await waitFor(() =>
      screen.getByRole("textbox", { name: "Reason to drop factory-16" }),
    );
    const submitDrop = within(reasonInput.closest("form")!).getByRole("button", { name: "Drop" });
    expect(submitDrop).toHaveProperty("disabled", true);
    fireEvent.change(reasonInput, { target: { value: "No longer needed" } });
    expect(submitDrop).toHaveProperty("disabled", false);
    fireEvent.click(submitDrop);

    await waitFor(() =>
      expect(
        calls.some(
          (c) =>
            c.method === "PATCH" &&
            c.path === "/v1/tasks/factory-16" &&
            (c.body as { status?: string; reason?: string }).status === "dropped" &&
            (c.body as { reason?: string }).reason === "No longer needed",
        ),
      ).toBe(true),
    );
  });

  test("surfaces an error notice and rolls back when a mutation fails", async () => {
    const { seed } = setupServer({ failPatch: true });
    await renderRoute("/boards/factory", { seed });
    await waitFor(() => screen.getByRole("region", { name: /Up Next/ }));

    fireEvent.click(screen.getByRole("button", { name: "Move factory-21 to Icebox" }));
    await waitFor(() =>
      expect(screen.getByText(/Task changed while you were viewing it/)).toBeTruthy(),
    );
    // Task rolled back into Up Next.
    expect(screen.getByText("Cache the last delta per agent")).toBeTruthy();
  });

  test("shows v2 metadata and operator controls on the Task Detail sidebar", async () => {
    const { state, calls, seed } = setupServer();
    await renderRoute("/tasks/factory-15", {
      seed: (client) => {
        seed(client);
        client.setQueryData(queries.task("factory-15").queryKey, state.task("factory-15"));
        client.setQueryData(
          queries.boardTasks("factory").queryKey,
          state.tasks(new URLSearchParams({ board: "factory" })),
        );
      },
    });

    const aside = await waitFor(() => screen.getByRole("complementary", { name: "Properties" }));
    expect(within(aside).getByText("#1")).toBeTruthy();
    expect(within(aside).getByText("User")).toBeTruthy();
    expect(within(aside).getByText("S")).toBeTruthy();

    // 1-click priority switch on Task Detail sidebar.
    fireEvent.click(within(aside).getByRole("button", { name: "Set factory-15 priority to P2" }));
    await waitFor(() =>
      expect(
        calls.some(
          (c) =>
            c.method === "PATCH" &&
            c.path === "/v1/tasks/factory-15" &&
            (c.body as { priority?: string }).priority === "p2",
        ),
      ).toBe(true),
    );
  });

  test("filters out uncooled tasks on Icebox tab when talking to a pre-v2 server", async () => {
    const state = new DevState(() => now);
    globalThis.fetch = (async (input: RequestInfo | URL) => {
      const rawUrl =
        typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
      const url = new URL(rawUrl, "http://localhost");
      if (url.pathname === "/v1/tasks") {
        // Simulate an older server that ignores `icebox=true` and returns all active tasks.
        const stripped = new URLSearchParams(url.searchParams);
        stripped.delete("icebox");
        return Response.json(state.tasks(stripped));
      }
      return Response.json([], { status: 404 });
    }) as typeof fetch;

    await renderRoute("/boards/factory?tab=icebox", {
      seed: (client) => {
        client.setQueryData(queries.boards().queryKey, state.boards());
        client.setQueryData(queries.openAsks().queryKey, []);
        client.setQueryData(queries.agents().queryKey, state.agents(new URLSearchParams()));
      },
    });

    await waitFor(() => expect(screen.getByText("Icebox is empty")).toBeTruthy());
  });
});
