import { describe, expect, test } from "bun:test";
import { renderInApp } from "../../test/render.tsx";
import { DevState } from "../../dev/state.ts";
import { HOSTILE_SAID, LONG_SAID } from "../../dev/fixtures.ts";
import { createApi } from "../api/client.ts";
import type { Updates } from "../api/schema.ts";
import { agentIndex } from "../ui/common.tsx";
import { isLong, markShownSeen, newestAt, unseenCount, UpdatesView } from "./updates.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const fresh = () => new DevState(() => now);
const state = fresh();
const agents = agentIndex(state.agents(new URLSearchParams()));
const updates = state.updates(new URLSearchParams());
const none: Updates = {
  seen_at: "2026-11-06T11:00:00Z",
  since: "2026-11-06T11:00:00Z",
  done: [],
  said: [],
  more: false,
};

const view = async (data: Updates, board = "") =>
  (await renderInApp(<UpdatesView updates={data} agents={agents} board={board} />)).container
    .innerHTML;

describe("updates section", () => {
  test("with nothing new it says so and offers no mark-seen", async () => {
    const html = await view(none);
    expect(html).toContain("You're up to date");
    expect(html).not.toContain("Mark all seen");
    expect(html).not.toContain("update-group");
  });

  test("lists finished tasks with link, board, owner handle, summary and PR and commit evidence", async () => {
    const html = await view(updates);
    expect(html).toContain("Mark all seen");
    expect(html).toContain('href="/tasks/web-view-5"');
    expect(html).toContain("Render agent markdown without HTML");
    expect(html).toContain('href="/boards/web-view"');
    expect(html).toContain(">claude:5be1</span>");
    // The summary is markdown: bold and code become elements.
    expect(html).toContain("<strong>Review the PR</strong>");
    expect(html).toContain('href="https://github.com/example/board-web/pull/12"');
    expect(html).toContain("<code>9b3f0d1</code>");
    // Checks are on the task page; the update shows what to open.
    expect(html).not.toContain("bun run check");
    // A task done without a summary, and its parent.
    expect(html).toContain("No summary written.");
    expect(html).toContain('href="/tasks/docs-1"');
  });

  test("lists coordinators' turn endings and collapses long ones", async () => {
    const html = await view(updates);
    expect(html).toContain("Coordinators said");
    expect(html).toContain(">t3:c2d0</span>");
    expect(isLong(LONG_SAID)).toBe(true);
    expect(html).toContain("data-clamped");
    expect(html).toContain('aria-expanded="false"');
    expect(html).toContain("Show all");
  });

  test("agent text stays inert: no script, no javascript: link, no remote image", async () => {
    const html = await view(updates);
    expect(html).toContain("Round done.");
    expect(HOSTILE_SAID).toContain("<script>");
    expect(html).not.toContain("<script");
    expect(html).not.toContain("<img");
    expect(html).not.toContain('href="javascript:');
    expect(html).toContain("&lt;/board&gt;");
  });

  test("says when older items were cut", async () => {
    expect(await view({ ...updates, more: true })).toContain("older ones were cut");
    expect(await view(updates)).not.toContain("older ones were cut");
  });

  test("with a board filter it cannot mark seen, since the cursor covers every board", async () => {
    const html = await view(state.updates(new URLSearchParams({ board: "docs" })), "docs");
    expect(html).not.toContain("Mark all seen");
    expect(html).toContain('href="/asks"');
  });

  test("counts what is unseen and finds the newest shown time", async () => {
    expect(unseenCount(updates)).toBe(updates.done.length + updates.said.length);
    expect(newestAt(none)).toBeNull();
    // The newest said (2 minutes ago) is newer than the newest done task (3 minutes ago).
    expect(newestAt(updates)).toBe(updates.said[0]?.at ?? "");
    const doneLater = { ...updates, said: [{ ...updates.said[0]!, at: "2026-11-06T10:00:00Z" }] };
    expect(newestAt(doneLater)).toBe(updates.done[0]?.done_at ?? "");
  });

  test("mark all seen sends the newest shown update's time, not the current time", async () => {
    const bodies: unknown[] = [];
    const client = createApi(async (_, init) => {
      bodies.push(JSON.parse(String(init?.body)));
      return Response.json({ seen_at: "2026-11-06T11:58:00Z" });
    });
    await markShownSeen(client, updates);
    expect(bodies).toEqual([{ at: updates.said[0]?.at }]);
    await markShownSeen(client, none);
    expect(bodies.length).toBe(1);
  });
});

describe("dev server seen cursor", () => {
  test("marking the newest shown time leaves only what came after", async () => {
    const dev = fresh();
    const shown = dev.updates(new URLSearchParams());
    const at = newestAt(shown) ?? "";
    expect(dev.markSeen({ at })).toEqual({ seen_at: at });
    expect(unseenCount(dev.updates(new URLSearchParams()))).toBe(0);
  });

  test("the cursor only moves forward and never past now", async () => {
    const dev = fresh();
    const before = dev.updates(new URLSearchParams()).seen_at;
    expect(dev.markSeen({ at: "2020-01-01T00:00:00Z" }).seen_at).toBe(before ?? "");
    expect(dev.markSeen({ at: "2030-01-01T00:00:00Z" }).seen_at).toBe(new Date(now).toISOString());
    expect(() => dev.markSeen({ at: "soon" })).toThrow("RFC 3339");
  });
});
