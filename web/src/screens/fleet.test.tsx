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
  client.setQueryData(
    queries.schedulerEvaluations(20).queryKey,
    state.schedulerEvaluations(new URLSearchParams()),
  );
}

describe("FleetScreen", () => {
  test("renders nodes, quota buckets, policy weights, telemetry, and autotune evaluations", async () => {
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
    // Automated evaluation & self-tuning panel.
    expect(screen.getByRole("heading", { name: /Automated evaluation/ })).toBeTruthy();
    expect(screen.getByText("mode: apply")).toBeTruthy();
    expect(screen.getByText("every 6h")).toBeTruthy();
    expect(screen.getAllByText("promoted").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("v1-default → v2-auto-20261106").length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("+20.9%").length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText("42.0s → 34.0s")).toBeTruthy();
    expect(screen.getByText("0.312 → 0.241")).toBeTruthy();
    expect(screen.getByText("keep")).toBeTruthy();
    // Devices with zero repos still disclose their capabilities.
    expect(screen.getAllByText("Capabilities").length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText("os:linux, arch:amd64")).toBeTruthy();
    // Verify zero inline style attributes for strict CSP compliance.
    expect(container.querySelectorAll("[style]").length).toBe(0);
  });

  test("renders empty state when no scheduler evaluations exist yet", async () => {
    await renderRoute("/fleet", {
      seed(client) {
        seed(client);
        client.setQueryData(queries.schedulerEvaluations(20).queryKey, {
          mode: "propose",
          enabled: false,
          interval_secs: 3600,
          evaluations: [],
        });
      },
    });
    expect(screen.getByText("paused")).toBeTruthy();
    expect(screen.getByText("mode: propose")).toBeTruthy();
    expect(screen.getByText("every 1h")).toBeTruthy();
    expect(screen.getByText(/No automated evaluations recorded yet/)).toBeTruthy();
  });

  test("preserves custom days query param in the telemetry window select", async () => {
    await renderRoute("/fleet?days=90", {
      seed(client) {
        seed(client);
        client.setQueryData(
          queries.schedulerStats(90).queryKey,
          state.schedulerStats(new URLSearchParams({ days: "90" })),
        );
      },
    });
    const select = screen.getByLabelText("Telemetry window") as HTMLSelectElement;
    expect(select.value).toBe("90");
    expect(screen.getByText("Last 90 days")).toBeTruthy();
  });
});
