import type { QueryClient } from "@tanstack/react-query";
import { screen } from "@testing-library/react";
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
  client.setQueryData(queries.devices().queryKey, state.devices());
  client.setQueryData(queries.usageBuckets().queryKey, state.usageBuckets());
  client.setQueryData(queries.schedulerPolicy().queryKey, state.schedulerPolicy());
  client.setQueryData(
    queries.schedulerStats(7).queryKey,
    state.schedulerStats(new URLSearchParams()),
  );
}

describe("FleetScreen", () => {
  test("renders nodes with roles, taints, capacity, quota buckets, policy weights, and telemetry", async () => {
    const { container } = await renderRoute("/fleet", { seed });
    expect(screen.getByRole("heading", { name: "Fleet" })).toBeTruthy();
    expect(screen.getByText("control-host")).toBeTruthy();
    expect(screen.getAllByText("mac-pro").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("role=control:no_schedule")).toBeTruthy();
    expect(screen.getByText("weight=heavy:prefer_no_schedule")).toBeTruthy();
    expect(screen.getByText("4 / 12 units (8 free)")).toBeTruthy();
    expect(screen.getByText("antigravity/default")).toBeTruthy();
    expect(screen.getByText("anthropic/default")).toBeTruthy();
    expect(screen.getByText("v1-default")).toBeTruthy();
    expect(screen.getByText("w_kind")).toBeTruthy();
    expect(screen.getByText("conflict_key: 2")).toBeTruthy();
    // Verify zero inline style attributes for strict CSP compliance.
    expect(container.querySelectorAll("[style]").length).toBe(0);
  });
});
