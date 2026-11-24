import type { QueryClient } from "@tanstack/react-query";
import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, test, vi } from "vite-plus/test";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { api } from "../api/client.ts";
import { queries } from "../live/query.ts";
import type { OpenSource, SourceHandlers } from "../live/stream.ts";
import { mergeRunEvents, RunDetailScreen } from "./runs.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);

function seed(client: QueryClient) {
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(queries.agents().queryKey, state.agents(new URLSearchParams()));
  client.setQueryData(queries.devices().queryKey, state.devices());
  client.setQueryData(
    queries.openAsks().queryKey,
    state.asks(new URLSearchParams({ status: "open" })),
  );
  client.setQueryData(
    queries.activeRuns().queryKey,
    state.runs(new URLSearchParams({ active: "true" })),
  );
  client.setQueryData(
    queries.runs({
      active: undefined,
      origin: undefined,
      kind: undefined,
      device: undefined,
      board: undefined,
      state: undefined,
      limit: 200,
    }).queryKey,
    state.runs(new URLSearchParams()),
  );
  client.setQueryData(queries.run("run-01a2b3c4").queryKey, state.run("run-01a2b3c4"));
  client.setQueryData(
    queries.runEvents("run-01a2b3c4").queryKey,
    state.runEvents("run-01a2b3c4", new URLSearchParams()),
  );
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("RunsScreen", () => {
  test("renders unified managed and interactive runs across devices", async () => {
    await renderRoute("/runs", { seed });
    expect(screen.getByRole("heading", { name: "Runs" })).toBeTruthy();
    expect(screen.getByText("Implement delta snapshot pagination")).toBeTruthy();
    expect(screen.getByText("Interactive cockpit polish")).toBeTruthy();
    expect(screen.getByText("Merge PR #41")).toBeTruthy();
    expect(screen.getAllByText("managed").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("interactive").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("runner-linux-1").length).toBeGreaterThanOrEqual(2);
    expect(screen.getAllByText("mac-pro").length).toBeGreaterThanOrEqual(2);
  });
});

describe("RunDetailScreen", () => {
  test("streams live SSE run events and supports steering and cancelling active runs", async () => {
    const user = userEvent.setup();
    let sseHandlers: SourceHandlers | null = null;
    const openSource: OpenSource = (_url, handlers) => {
      sseHandlers = handlers;
      return { close: () => {} };
    };

    vi.spyOn(api, "steerRun").mockResolvedValue({
      wake_id: 42,
      run_id: "run-01a2b3c4",
      device: "runner-linux-1",
    });
    vi.spyOn(api, "cancelRun").mockResolvedValue({
      run_id: "run-01a2b3c4",
      device: "runner-linux-1",
      state: "running",
      wake_id: 43,
    });

    // Render RunDetailScreen directly through router using a custom route helper? Use renderInApp?
    // RunDetailScreen uses runDetailRoute.useParams(), so mock global EventSource instead via renderRoute!
    const origES = globalThis.EventSource;
    class FakeEventSource {
      onopen: (() => void) | null = null;
      onmessage: ((ev: MessageEvent<string>) => void) | null = null;
      onerror: (() => void) | null = null;
      constructor(public readonly url: string) {
        sseHandlers = {
          open: () => this.onopen?.(),
          message: (data: string) => this.onmessage?.({ data } as MessageEvent<string>),
          error: () => this.onerror?.(),
        };
      }
      close() {}
    }
    globalThis.EventSource = FakeEventSource as unknown as typeof EventSource;
    try {
      await renderRoute("/runs/run-01a2b3c4", { seed });
      expect(screen.getByText("Implement delta snapshot pagination")).toBeTruthy();
      expect(screen.getByText("Prepared worktree feat/factory-12-delta")).toBeTruthy();

      // Deliver a live SSE event and verify it appends cleanly.
      act(() => {
        sseHandlers?.open();
        sseHandlers?.message(
          JSON.stringify({
            run_id: "run-01a2b3c4",
            seq: 4,
            at: "2026-11-06T12:00:00Z",
            kind: "pr",
            summary: "Opened PR #99",
            meta: null,
          }),
        );
      });
      expect(screen.getByText("Opened PR #99")).toBeTruthy();

      // Steer the run.
      const box = screen.getByLabelText("Steering message");
      await user.type(box, "focus on edge cases");
      await user.click(screen.getByRole("button", { name: "Send steer" }));
      await waitFor(() => expect(screen.getByText(/wake #42/)).toBeTruthy());
      expect(api.steerRun).toHaveBeenCalledWith("run-01a2b3c4", "focus on edge cases");

      // Cancel the run via dialog.
      await user.click(screen.getByRole("button", { name: "Cancel run" }));
      await user.type(screen.getByLabelText("Reason (optional)"), "superseded");
      const buttons = screen.getAllByRole("button", { name: "Cancel run" });
      await user.click(buttons[buttons.length - 1]!);
      await waitFor(() =>
        expect(screen.getByText(/Cancel requested for run-01a2b3c4/)).toBeTruthy(),
      );
      expect(api.cancelRun).toHaveBeenCalledWith("run-01a2b3c4", "superseded");
      void openSource;
      void RunDetailScreen;
    } finally {
      globalThis.EventSource = origES;
    }
  });

  test("mergeRunEvents deduplicates by seq and caps at 200 latest events", () => {
    const base = Array.from({ length: 199 }, (_, i) => ({
      run_id: "run-1",
      seq: i + 1,
      at: "2026-11-06T12:00:00Z",
      kind: "note" as const,
      summary: `ev ${i + 1}`,
      meta: null,
    }));
    const merged = mergeRunEvents(base, [
      { ...base[198]!, summary: "updated 199" },
      { ...base[0]!, seq: 200, summary: "ev 200" },
      { ...base[0]!, seq: 201, summary: "ev 201" },
    ]);
    expect(merged.length).toBe(200);
    expect(merged[0]?.seq).toBe(2);
    expect(merged.at(-1)?.seq).toBe(201);
    expect(merged.find((e) => e.seq === 199)?.summary).toBe("updated 199");
  });
});
