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
  client.setQueryData(
    queries.healthOverview(7).queryKey,
    state.healthOverview(new URLSearchParams()),
  );
}

describe("Fleet redirect to HealthScreen", () => {
  test("redirects /fleet to /health and renders nodes, quota buckets, policy weights, telemetry, and autotune evaluations", async () => {
    const { container } = await renderRoute("/fleet", { seed });
    expect(screen.getByRole("heading", { name: "System Health" })).toBeTruthy();
    expect(screen.getAllByText("control-host").length).toBeGreaterThanOrEqual(1);
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
    // Devices disclose their runners/repos/capabilities.
    expect(screen.getAllByText(/os:linux, arch:amd64/).length).toBeGreaterThanOrEqual(1);
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
          queries.healthOverview(90).queryKey,
          state.healthOverview(new URLSearchParams({ days: "90" })),
        );
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

  test("renders pace hold, next day, stale state, boundary rollover, and hard line on QuotaBucketCard", async () => {
    const liveNow = Date.now();
    const iso = (deltaMs: number) => new Date(liveNow + deltaMs).toISOString();
    const hour = 60 * 60 * 1000;
    const min = 60 * 1000;
    await renderRoute("/fleet", {
      seed(client) {
        seed(client);
        client.setQueryData(queries.usageBuckets().queryKey, [
          {
            provider: "antigravity",
            account: "default",
            five_hour: { percent: 2.0, resets_at: iso(3 * hour) },
            seven_day: { percent: 23.4, resets_at: iso(6 * 24 * hour + 5 * hour) },
            read_at: iso(-2 * min),
            device: "mac-pro",
            reserve: 5.0,
            pace: {
              reserve: 5.0,
              day_start: iso(-19 * hour),
              anchor: 0.4,
              line: 13.9,
              projected: 23.4,
              hard_since: null,
            },
            reserve_until: null,
            reserve_prev: null,
          },
          {
            provider: "anthropic",
            account: "default",
            five_hour: { percent: 31.0, resets_at: iso(-48 * hour) },
            seven_day: { percent: 32.7, resets_at: iso(3 * 24 * hour) },
            read_at: iso(-72 * hour),
            device: "mac-pro",
            reserve: 5.0,
            pace: {
              reserve: 5.0,
              day_start: iso(-75 * hour),
              anchor: 2.9,
              line: 16.0,
              projected: 33.0,
              hard_since: null,
            },
            reserve_until: null,
            reserve_prev: null,
          },
          {
            provider: "openai",
            account: "default",
            five_hour: null,
            seven_day: { percent: 20.0, resets_at: iso(5 * 24 * hour) },
            read_at: iso(-2 * min),
            device: "mac-pro",
            reserve: 5.0,
            pace: {
              reserve: 5.0,
              day_start: iso(-24 * hour - min),
              anchor: 1.0,
              line: 15.0,
              projected: 20.0,
              hard_since: null,
            },
            reserve_until: iso(2 * hour),
            reserve_prev: 10.0,
          },
          {
            provider: "google",
            account: "hard",
            five_hour: { percent: 92.0, resets_at: iso(-2 * min) },
            seven_day: { percent: 96.0, resets_at: iso(3 * 24 * hour) },
            read_at: iso(-5 * min),
            device: "mac-pro",
            reserve: 5.0,
            pace: {
              reserve: 5.0,
              day_start: iso(-26 * hour),
              anchor: 80.0,
              line: 85.0,
              projected: 96.0,
              hard_since: iso(-2 * hour),
            },
            reserve_until: null,
            reserve_prev: null,
          },
        ]);
      },
    });
    expect(screen.getByText("Pace hold")).toBeTruthy();
    expect(screen.getByText(/Next day/)).toBeTruthy();
    expect(screen.getByText("Stale")).toBeTruthy();
    expect(screen.getByText("Pace inactive (stale reading)")).toBeTruthy();
    expect(screen.getByText("New pace day started · awaiting reading")).toBeTruthy();
    expect(screen.getByText(/Reserve 5\.0% \(until/)).toBeTruthy();
    expect(screen.getByText("Hard line")).toBeTruthy();
    expect(screen.getByText("Hard line 95.0% · holds until weekly reset")).toBeTruthy();
  });
});
