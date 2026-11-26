import type { QueryClient } from "@tanstack/react-query";
import { fireEvent, screen } from "@testing-library/react";
import { describe, expect, test } from "vite-plus/test";
import { DevState } from "../../dev/state.ts";
import { renderRoute } from "../../test/render.tsx";
import { queries } from "../live/query.ts";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);

function seed(client: QueryClient) {
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(queries.agents().queryKey, state.agents(new URLSearchParams()));
  client.setQueryData(
    queries.openAsks().queryKey,
    state.asks(new URLSearchParams({ status: "open" })),
  );
  client.setQueryData(
    queries.activeRuns().queryKey,
    state.runs(new URLSearchParams({ active: "true" })),
  );
  client.setQueryData(queries.schedules({}).queryKey, state.schedules(new URLSearchParams()));
  client.setQueryData(
    queries.scheduleHistory("mac-pro:launchd:com.example.storage.prune").queryKey,
    state.scheduleHistory("mac-pro:launchd:com.example.storage.prune"),
  );
}

describe("SchedulesScreen", () => {
  test("renders KPI strip, grouped table, OS hidden badge, detail drawer, and 24h timeline with zero inline styles", async () => {
    const { container } = await renderRoute("/schedules", { seed });

    expect(screen.getByRole("heading", { name: "Schedules" })).toBeTruthy();
    expect(screen.getByText("Storage repository prune")).toBeTruthy();
    expect(screen.getByText("Daily engineering digest")).toBeTruthy();
    expect(screen.getByText("TLS certificate check")).toBeTruthy();
    // OS timer hidden by default
    expect(screen.queryByText("Daily apt download activities")).toBeNull();
    expect(screen.getByText("(1)")).toBeTruthy();

    // Open detail drawer for the failing job
    fireEvent.click(screen.getByRole("button", { name: "Storage repository prune" }));
    expect(await screen.findByText("Last error output (redacted)")).toBeTruthy();
    expect(screen.getAllByText(/stale lock held by old PID/).length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText(/Recent run & state history/)).toBeTruthy();

    // Switch to 24h timeline view
    fireEvent.click(screen.getByRole("button", { name: "Close job details" }));
    fireEvent.click(screen.getByRole("tab", { name: "24h Timeline" }));
    expect(
      await screen.findByLabelText("24-hour JST scheduled jobs cadence timeline"),
    ).toBeTruthy();

    // Strict CSP compliance
    expect(container.querySelectorAll("[style]").length).toBe(0);
  });
});
