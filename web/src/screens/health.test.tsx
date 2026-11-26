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

describe("HealthScreen", () => {
  test("renders all 7 pillars, separates Watchdog, switches token breakdown, and keeps zero inline styles", async () => {
    const { container } = await renderRoute("/health", { seed });

    // Top posture & incident chips
    expect(screen.getByRole("heading", { name: "System Health" })).toBeTruthy();
    expect(screen.getByText("Degraded")).toBeTruthy();
    expect(screen.getByText("Degraded cause:")).toBeTruthy();
    expect(screen.getByText(/Version skew on board/)).toBeTruthy();
    expect(screen.getByText("Watchdog alive")).toBeTruthy();
    expect(screen.getByText("Alert pipeline alive (Watchdog active)")).toBeTruthy();

    // Version skew banner + host cards + services
    expect(screen.getByText("Version skew detected")).toBeTruthy();
    expect(screen.getByText("board-server")).toBeTruthy();
    expect(screen.getByText("prometheus.service")).toBeTruthy();

    // Token analytics & breakdown tab switching
    expect(screen.getByText("Prompt cache hit")).toBeTruthy();
    expect(screen.getByText("84.0%")).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "By model" }));
    expect(screen.getByText("gemini-4-neon")).toBeTruthy();

    // Storage, backups, Vault, CI, Certs, External alerts
    expect(screen.getByText("media-cold")).toBeTruthy();
    expect(screen.getByText("PostgreSQL primary")).toBeTruthy();
    expect(screen.getByText("StorageQuotaHigh")).toBeTruthy();
    expect(screen.getByText("Vault Internal CA")).toBeTruthy();
    expect(screen.getByText("unsealed")).toBeTruthy();

    // Strict CSP compliance
    expect(container.querySelectorAll("[style]").length).toBe(0);
  });

  test("omits token-only offline control devices from host cards and excludes clean-exited containers from problems", async () => {
    const baseOverview = state.healthOverview(new URLSearchParams());
    const baseDevices = state.devices();
    await renderRoute("/health", {
      seed(client) {
        seed(client);
        client.setQueryData(queries.devices().queryKey, [
          ...baseDevices,
          {
            name: "mcp-http",
            role: "control",
            status: "offline",
            drain: false,
            os: null,
            arch: null,
            capacity: null,
            free_units: null,
            capabilities: [],
            runners: [],
            repos: [],
            taints: [{ key: "role", value: "control", effect: "no_schedule" }],
            preference: 0,
            protocol_features: [],
            heartbeat_at: null,
          },
        ]);
        client.setQueryData(queries.healthOverview(7).queryKey, {
          ...baseOverview,
          posture: "incident",
          posture_reasons: ["Control host offline: ctrl-real"],
          hosts: baseOverview.hosts.map((h, idx) =>
            idx === 0
              ? {
                  ...h,
                  containers: [
                    ...h.containers,
                    {
                      name: "dokploy.1.old-task",
                      image: "dokploy/dokploy:v0.31.4",
                      state: "exited",
                      health: "unhealthy",
                      uptime_secs: null,
                      restarts: 0,
                      status_text: "Exited (0) About an hour ago",
                    },
                  ],
                }
              : h,
          ),
        });
      },
    });

    expect(screen.getByText("Incident cause:")).toBeTruthy();
    expect(screen.getByText("Control host offline: ctrl-real")).toBeTruthy();
    expect(screen.queryByText("mcp-http")).toBeNull();
    expect(screen.getByText("dokploy.1.old-task")).toBeTruthy();
    expect(screen.getByText("exited (0)")).toBeTruthy();

    fireEvent.click(screen.getByRole("checkbox", { name: "Problems only" }));
    expect(screen.queryByText("dokploy.1.old-task")).toBeNull();
  });
});
