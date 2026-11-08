import { afterEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { act, screen, waitFor, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();
const open = state.asks(new URLSearchParams({ status: "open" }));
const decision = open.find((ask) => ask.kind === "decision" && ask.task !== null)!;
const action = open.find((ask) => ask.kind === "action" && ask.task !== null)!;

const original = globalThis.fetch;
afterEach(() => {
  globalThis.fetch = original;
});

/** Answers each API call from the fixtures; `hold` keeps a path's answer back until released. */
function serve(hold?: string) {
  const asked: string[] = [];
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const path = new URL(String(input), "http://localhost").pathname;
    asked.push(path);
    if (path === hold) await held;
    const ask = /^\/v1\/asks\/(\d+)$/.exec(path);
    if (ask) return Response.json(state.ask(Number(ask[1])));
    return Response.json(
      { code: "not_found", message: `No fixture for ${path}.`, next: null },
      { status: 404 },
    );
  }) as typeof fetch;
  return { asked, release };
}

/** Everything the shell and the ask's context read, so only the ask itself is fetched. */
function seed(askId: number) {
  return (client: QueryClient) => {
    const ask = state.ask(askId);
    client.setQueryData(queries.boards().queryKey, state.boards());
    client.setQueryData(queries.openAsks().queryKey, open);
    client.setQueryData(queries.agents().queryKey, state.agents(none));
    if (ask.task) client.setQueryData(queries.task(ask.task).queryKey, state.task(ask.task));
    client.setQueryData(queries.handoff(ask.agent).queryKey, state.handoff(ask.agent));
  };
}

const title = () => screen.getByRole("heading", { level: 1 }).textContent;
const current = () =>
  within(screen.getByRole("navigation", { name: "Breadcrumb" })).getByText(/^Ask #/).textContent;

describe("a single ask's page", () => {
  test("is titled by number while loading, then by its kind and number", async () => {
    const server = serve(`/v1/asks/${decision.id}`);
    await renderRoute(`/asks/${decision.id}`, { seed: seed(decision.id) });
    expect(title()).toBe(`Ask #${decision.id}`);
    expect(document.title).toBe(`Ask #${decision.id} · agent-board`);

    await act(async () => server.release());
    await waitFor(() => expect(title()).toBe(`Decision · Ask #${decision.id}`));
    expect(document.title).toBe(`Decision · Ask #${decision.id} · agent-board`);
    // The crumb stays the plain number; the kind is said once, in the title.
    expect(current()).toBe(`Ask #${decision.id}`);
    expect(screen.queryByText("Decision", { exact: true })).toBeNull();
    expect(server.asked).toEqual([`/v1/asks/${decision.id}`]);
  });

  test("an action ask is titled as one", async () => {
    serve();
    await renderRoute(`/asks/${action.id}`, { seed: seed(action.id) });
    await waitFor(() => expect(title()).toBe(`Action · Ask #${action.id}`));
    expect(screen.queryByText("Action", { exact: true })).toBeNull();
  });

  test("shows the answer first, then the task and the asking agent's handoff", async () => {
    serve();
    await renderRoute(`/asks/${decision.id}`, { seed: seed(decision.id) });
    const send = await screen.findByRole("button", { name: "Send answer" });
    const about = screen.getByRole("complementary", { name: "About this ask" });
    const field = screen.getByRole("textbox", { name: "Or write your own answer" });
    const follows = (a: Node, b: Node) =>
      Boolean(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);
    // Send closes the answer group: after every option and the text field.
    for (const option of screen.getAllByRole("radio")) expect(follows(option, send)).toBe(true);
    expect(follows(field, send)).toBe(true);
    expect(follows(send, about)).toBe(true);
    expect(within(about).getByText(state.task(decision.task!).task.title)).toBeTruthy();
    expect(within(about).getByRole("region", { name: "Handoff" })).toBeTruthy();
  });

  test("a link that is not an ask number keeps the number as typed and says so", async () => {
    const server = serve();
    await renderRoute("/asks/abc", { seed: seed(decision.id) });
    expect(title()).toBe("Ask #abc");
    expect(screen.getByText("“abc” is not an ask number")).toBeTruthy();
    expect(server.asked).toEqual([]);
  });
});
