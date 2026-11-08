import { afterEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";
import { setConnection, stream } from "../live/runtime.ts";
import type { ConnectionState } from "../live/stream.ts";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();

// The dev server's simulated outage answer (`/__dev/outage?on=1`).
const MESSAGE = "The board database is unavailable (simulated outage).";
const NEXT = "retry in a minute";
const PLACEHOLDER = "This page loads again once the connection is back.";
const BANNERS = {
  reconnecting: "Lost the connection to the board server.",
  offline: "You're offline.",
};

const original = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = original;
  act(() => setConnection({ kind: "connecting" }));
});

/** Every API call fails as the server does in an outage; the calls are counted. */
function outage() {
  const calls: string[] = [];
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    calls.push(new URL(String(input), "http://localhost").pathname);
    return Response.json({ code: "internal", message: MESSAGE, next: NEXT }, { status: 500 });
  }) as typeof fetch;
  return calls;
}

/** The shell's data is loaded; the board's tasks are what fails. */
function seed(client: QueryClient) {
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(
    queries.openAsks().queryKey,
    state.asks(new URLSearchParams({ status: "open" })),
  );
  client.setQueryData(queries.agents().queryKey, state.agents(none));
}

async function failedBoard(connection: ConnectionState) {
  const calls = outage();
  act(() => setConnection(connection));
  await renderRoute("/boards/factory", { seed });
  await screen.findByText(MESSAGE);
  return calls;
}

const card = () => screen.getByText(MESSAGE).closest("[data-slot=alert]") as HTMLElement;
// The banner, not the sidebar's connection pill, which is a status too.
const banner = () =>
  screen
    .queryAllByRole("status")
    .find((node) => Object.values(BANNERS).some((text) => node.textContent?.includes(text)));

describe("a page that fails to load", () => {
  test.each([
    ["reconnecting", { kind: "reconnecting", attempt: 2, retryAt: now } as const],
    ["offline", { kind: "offline" } as const],
  ])("while %s, only the banner warns; the page is a quiet placeholder", async (kind, state) => {
    await failedBoard(state);
    expect(banner()?.textContent).toContain(BANNERS[kind as keyof typeof BANNERS]);
    // One alarm: the card announces itself as news, not as a second alert.
    expect(card().getAttribute("role")).toBe("status");
    expect(screen.queryAllByRole("alert")).toEqual([]);
    expect(card().textContent).toContain(PLACEHOLDER);
    // The server's own advice repeats what the banner says, so it gives way to the placeholder.
    expect(card().textContent).not.toContain(NEXT);
    // No warning colour or rail on the card: the banner carries the tone.
    expect(card().className).not.toMatch(/error|warn|amber/);
    // Its icon is still tinted, so it does not read as an empty state.
    expect(card().querySelector("[data-slot=failure-icon]")?.className).toContain("bg-error-soft");
  });

  test.each([
    ["live", { kind: "live" } as const],
    ["connecting", { kind: "connecting" } as const],
  ])("while %s, the card alerts with the server's message and advice", async (_, state) => {
    await failedBoard(state);
    expect(banner()).toBeUndefined();
    expect(screen.getAllByRole("alert")).toEqual([card()]);
    expect(card().textContent).toContain(NEXT);
    expect(card().textContent).not.toContain(PLACEHOLDER);
    expect(card().className).toContain("before:bg-error");
  });

  test("the card turns back into an alert when the connection returns without the data", async () => {
    await failedBoard({ kind: "offline" });
    expect(card().getAttribute("role")).toBe("status");
    act(() => setConnection({ kind: "live" }));
    expect(card().getAttribute("role")).toBe("alert");
    expect(card().textContent).toContain(NEXT);
    expect(banner()).toBeUndefined();
  });

  test("the live stream's state is what the page shows", async () => {
    outage();
    await renderRoute("/boards/factory", { seed });
    await screen.findByText(MESSAGE);
    expect(card().getAttribute("role")).toBe("alert");
    // The browser reports no network, so the stream goes offline without trying to connect.
    Object.defineProperty(navigator, "onLine", { configurable: true, get: () => false });
    try {
      act(() => stream.start());
      await waitFor(() => expect(banner()?.textContent).toContain(BANNERS.offline));
      expect(card().getAttribute("role")).toBe("status");
    } finally {
      stream.stop();
      Reflect.deleteProperty(navigator, "onLine");
    }
  });

  test("with nothing loaded, the sidebar shows placeholder boards and an offline asks mark", async () => {
    outage();
    act(() => setConnection({ kind: "offline" }));
    await renderRoute("/boards/factory");
    await screen.findByText(MESSAGE);
    await screen.findByText("Boards show again once the connection is back.");
    const boards = screen.getByRole("navigation", { name: "Boards" });
    expect(boards.querySelectorAll("ul[aria-hidden=true] > li").length).toBe(3);
    const asks = await screen.findByRole("img", {
      name: "Count unknown until the connection is back",
    });
    expect(asks.closest("a")?.textContent).toBe("Needs you");
    expect(asks.className).not.toMatch(/bg-amber/);
  });

  test("Try again is the card's primary action and asks the server again", async () => {
    const calls = await failedBoard({ kind: "reconnecting", attempt: 1, retryAt: now });
    const before = calls.filter((path) => path === "/v1/tasks").length;
    expect(before).toBe(1);
    const retry = screen.getByRole("button", { name: "Try again" });
    expect(retry.getAttribute("data-variant")).toBe("default");
    await act(() => fireEvent.click(retry));
    await waitFor(() => expect(calls.filter((path) => path === "/v1/tasks").length).toBe(2));
  });
});
