import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getRouteApi, Link, useNavigate } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import { api } from "@/api/client.ts";
import type {
  AlertSeverity,
  BackupState,
  DeviceEntry,
  HealthOverview,
  HealthPosture,
  HostHealthSummary,
  HostResourceSample,
  TokenBreakdownRow,
} from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import { NativeSelectOption } from "@/components/ui/native-select.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { useNow } from "@/live/runtime.ts";
import {
  AbsTime,
  Empty,
  ErrorNotice,
  formatUntil,
  PageHeader,
  QueryFrame,
  RelTime,
  SectionTitle,
  useTitle,
} from "@/ui/common.tsx";
import { FilterSelect, FilterToggle } from "@/ui/filters.tsx";
import { Icon } from "@/ui/icons.tsx";
import {
  DeviceCard,
  DeviceStatusPill,
  formatPct,
  formatTokens,
  MeterBar,
  MetricTile,
  PolicyCard,
  QuotaBucketCard,
  SchedulerAutotuneView,
  SchedulerTelemetryView,
} from "./fleet.tsx";

const healthRoute = getRouteApi("/health");

const WINDOW_DAYS = [1, 7, 14, 30] as const;

function parseDays(raw: string | undefined): number {
  const n = Number(raw);
  return Number.isInteger(n) && n >= 1 && n <= 365 ? n : 7;
}

function formatBytes(bytes: number | null): string {
  if (bytes === null || !Number.isFinite(bytes)) return "—";
  if (bytes >= 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${bytes} B`;
}

function formatDuration(secs: number | null): string {
  if (secs === null || !Number.isFinite(secs) || secs < 0) return "—";
  if (secs < 60) return `${Math.round(secs)}s`;
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  if (secs < 86_400) {
    const h = Math.floor(secs / 3600);
    const m = Math.round((secs % 3600) / 60);
    return m > 0 ? `${h}h ${m}m` : `${h}h`;
  }
  const d = Math.floor(secs / 86_400);
  const h = Math.floor((secs % 86_400) / 3600);
  return h > 0 ? `${d}d ${h}h` : `${d}d`;
}

function postureMeta(posture: HealthPosture): { label: string; pill: string; dot: string } {
  switch (posture) {
    case "healthy":
      return {
        label: "Healthy",
        pill: "border-status-ready/35 bg-status-ready-soft text-status-ready",
        dot: "bg-status-ready",
      };
    case "degraded":
      return {
        label: "Degraded",
        pill: "border-amber/35 bg-amber-soft text-amber",
        dot: "bg-amber",
      };
    case "incident":
      return {
        label: "Incident",
        pill: "border-status-blocked/35 bg-status-blocked-soft text-status-blocked",
        dot: "bg-status-blocked",
      };
    default:
      return {
        label: "Unknown",
        pill: "border-border bg-secondary text-muted-foreground",
        dot: "bg-muted-foreground",
      };
  }
}

function serviceTone(state: string): {
  label: string;
  variant: "ok" | "bad" | "warn" | "outline";
  rank: number;
} {
  const s = state.toLowerCase();
  if (s === "failed" || s === "unhealthy" || s === "exited" || s === "dead") {
    return { label: state, variant: "bad", rank: 0 };
  }
  if (s === "degraded" || s === "restarting" || s === "starting" || s === "stopped") {
    return { label: state, variant: "warn", rank: 1 };
  }
  if (s === "running" || s === "healthy") {
    return { label: state, variant: "ok", rank: 2 };
  }
  return { label: state, variant: "outline", rank: 1 };
}

function backupVariant(state: BackupState): "ok" | "warn" | "bad" | "outline" {
  switch (state) {
    case "ok":
      return "ok";
    case "stale":
      return "warn";
    case "failed":
      return "bad";
    default:
      return "outline";
  }
}

function severityVariant(severity: AlertSeverity): "bad" | "warn" | "accent" | "outline" {
  switch (severity) {
    case "critical":
      return "bad";
    case "warning":
      return "warn";
    case "info":
      return "accent";
    default:
      return "outline";
  }
}

/** CSP-safe SVG sparkline of recent hourly CPU & RAM utilization samples. */
function HostSparkline({ samples, host }: { samples: HostResourceSample[]; host: string }) {
  if (samples.length < 2) return null;
  const w = 160;
  const h = 32;
  const pad = 2;
  const points = (pick: (s: HostResourceSample) => number | null) => {
    const coords: string[] = [];
    for (let i = 0; i < samples.length; i++) {
      const sample = samples[i];
      if (!sample) continue;
      const val = pick(sample);
      if (val === null || !Number.isFinite(val)) continue;
      const x = pad + (i / (samples.length - 1)) * (w - pad * 2);
      const y = h - pad - (Math.max(0, Math.min(100, val)) / 100) * (h - pad * 2);
      coords.push(`${x.toFixed(1)},${y.toFixed(1)}`);
    }
    return coords.join(" ");
  };
  const cpuPts = points((s) => s.cpu_pct);
  const memPts = points((s) => s.mem_used_pct);
  if (!cpuPts && !memPts) return null;

  return (
    <div className="grid gap-1">
      <div className="flex items-center justify-between text-2xs text-muted-foreground">
        <span>Hourly trend ({samples.length}h)</span>
        <span className="flex items-center gap-2">
          <span className="inline-flex items-center gap-1">
            <span className="size-1.5 rounded-full bg-status-doing" aria-hidden="true" />
            CPU
          </span>
          <span className="inline-flex items-center gap-1">
            <span className="size-1.5 rounded-full bg-status-review" aria-hidden="true" />
            RAM
          </span>
        </span>
      </div>
      <svg
        viewBox={`0 0 ${w} ${h}`}
        className="h-8 w-full rounded-sm bg-muted/70"
        role="img"
        aria-label={`${host} hourly CPU and memory trend`}
      >
        <line x1={0} x2={w} y1={h / 2} y2={h / 2} className="stroke-border stroke-[0.6]" />
        {memPts && (
          <polyline
            fill="none"
            points={memPts}
            className="stroke-status-review stroke-[1.4] [stroke-linecap:round] [stroke-linejoin:round]"
          />
        )}
        {cpuPts && (
          <polyline
            fill="none"
            points={cpuPts}
            className="stroke-status-doing stroke-[1.5] [stroke-linecap:round] [stroke-linejoin:round]"
          />
        )}
      </svg>
    </div>
  );
}

/** CSP-safe SVG stacked bar chart for daily token consumption + cache hit % overlay. */
function TokenTrendChart({ overview }: { overview: HealthOverview }) {
  const daily = overview.tokens.daily;
  if (daily.length === 0) {
    return (
      <p className="py-6 text-center text-meta text-muted-foreground">
        No daily token samples recorded in this window.
      </p>
    );
  }

  const width = 640;
  const height = 180;
  const left = 44;
  const right = 38;
  const top = 14;
  const bottom = 26;
  const plotW = width - left - right;
  const plotH = height - top - bottom;
  const maxTotal = Math.max(1, ...daily.map((d) => d.cached_tokens + d.billable_in + d.tokens_out));
  const step = plotW / daily.length;
  const barW = Math.max(6, Math.min(32, step * 0.62));

  const hitPoints = daily
    .map((d, i) => {
      if (d.cache_hit_rate === null || !Number.isFinite(d.cache_hit_rate)) return null;
      const cx = left + i * step + step / 2;
      const cy = top + plotH - Math.max(0, Math.min(1, d.cache_hit_rate)) * plotH;
      return `${cx.toFixed(1)},${cy.toFixed(1)}`;
    })
    .filter((p): p is string => p !== null)
    .join(" ");

  const labelStep = Math.max(1, Math.ceil(daily.length / 7));

  return (
    <div className="grid gap-2">
      <div className="flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground">
        <span className="font-semibold text-subtle-foreground">
          Daily token volume &amp; cache hit rate
        </span>
        <div className="flex flex-wrap items-center gap-3">
          <span className="inline-flex items-center gap-1.5">
            <span className="size-2 rounded-[2px] bg-status-ready" aria-hidden="true" />
            Cached in
          </span>
          <span className="inline-flex items-center gap-1.5">
            <span className="size-2 rounded-[2px] bg-status-doing" aria-hidden="true" />
            Billable in
          </span>
          <span className="inline-flex items-center gap-1.5">
            <span className="size-2 rounded-[2px] bg-status-review" aria-hidden="true" />
            Output
          </span>
          <span className="inline-flex items-center gap-1.5">
            <span className="h-0.5 w-3 rounded-full bg-amber" aria-hidden="true" />
            Cache hit %
          </span>
        </div>
      </div>

      <svg
        viewBox={`0 0 ${width} ${height}`}
        className="h-44 w-full"
        role="img"
        aria-label="Daily token usage and cache hit rate trend"
      >
        {/* Horizontal gridlines */}
        {[0, 0.5, 1].map((frac) => {
          const y = top + plotH * (1 - frac);
          return (
            <g key={frac}>
              <line
                x1={left}
                x2={width - right}
                y1={y}
                y2={y}
                className="stroke-border stroke-[0.75]"
              />
              <text
                x={left - 6}
                y={y + 3}
                textAnchor="end"
                className="fill-muted-foreground text-[9px] tabular-nums"
              >
                {formatTokens(Math.round(maxTotal * frac))}
              </text>
              <text
                x={width - right + 6}
                y={y + 3}
                textAnchor="start"
                className="fill-muted-foreground text-[9px] tabular-nums"
              >
                {Math.round(frac * 100)}%
              </text>
            </g>
          );
        })}

        {/* Stacked daily bars */}
        {daily.map((d, i) => {
          const x = left + i * step + (step - barW) / 2;
          const cachedH = (d.cached_tokens / maxTotal) * plotH;
          const billableH = (d.billable_in / maxTotal) * plotH;
          const outH = (d.tokens_out / maxTotal) * plotH;
          const yCached = top + plotH - cachedH;
          const yBillable = yCached - billableH;
          const yOut = yBillable - outH;
          const shortDay = d.day.slice(5);
          return (
            <g key={d.day}>
              <title>
                {`${d.day}: ${d.runs} runs · in ${formatTokens(d.tokens_in)} (cached ${formatTokens(d.cached_tokens)}, billable ${formatTokens(d.billable_in)}) · out ${formatTokens(d.tokens_out)} · cache hit ${formatPct(d.cache_hit_rate)}`}
              </title>
              {cachedH > 0 && (
                <rect
                  x={x}
                  y={yCached}
                  width={barW}
                  height={cachedH}
                  className="fill-status-ready/75"
                />
              )}
              {billableH > 0 && (
                <rect
                  x={x}
                  y={yBillable}
                  width={barW}
                  height={billableH}
                  className="fill-status-doing"
                />
              )}
              {outH > 0 && (
                <rect x={x} y={yOut} width={barW} height={outH} className="fill-status-review" />
              )}
              {i % labelStep === 0 && (
                <text
                  x={left + i * step + step / 2}
                  y={height - 7}
                  textAnchor="middle"
                  className="fill-muted-foreground text-[9px] tabular-nums"
                >
                  {shortDay}
                </text>
              )}
            </g>
          );
        })}

        {/* Cache hit rate overlay line */}
        {hitPoints && (
          <polyline
            fill="none"
            points={hitPoints}
            className="stroke-amber stroke-[1.8] [stroke-linecap:round] [stroke-linejoin:round]"
          />
        )}
        {daily.map((d, i) => {
          if (d.cache_hit_rate === null || !Number.isFinite(d.cache_hit_rate)) return null;
          const cx = left + i * step + step / 2;
          const cy = top + plotH - Math.max(0, Math.min(1, d.cache_hit_rate)) * plotH;
          return <circle key={`pt-${d.day}`} cx={cx} cy={cy} r={2.5} className="fill-amber" />;
        })}
      </svg>
    </div>
  );
}

type TokenDimension = "board" | "kind" | "device" | "model";

function TokenBreakdownTable({ overview }: { overview: HealthOverview }) {
  const [dim, setDim] = useState<TokenDimension>("board");
  const rows: TokenBreakdownRow[] =
    dim === "board"
      ? overview.tokens.by_board
      : dim === "kind"
        ? overview.tokens.by_kind
        : dim === "device"
          ? overview.tokens.by_device
          : overview.tokens.by_model;

  return (
    <div className="overflow-x-auto rounded-lg border bg-card">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b px-3.5 py-2.5">
        <h3 className="text-xs font-semibold text-subtle-foreground">Token breakdown</h3>
        <div className="inline-flex rounded-md border bg-muted p-0.5 text-xs" role="tablist">
          {(
            [
              ["board", "By board"],
              ["kind", "By kind"],
              ["device", "By device"],
              ["model", "By model"],
            ] as const
          ).map(([key, label]) => (
            <button
              key={key}
              type="button"
              role="tab"
              aria-selected={dim === key}
              onClick={() => setDim(key)}
              className={cn(
                "cursor-pointer rounded-sm px-2 py-0.5 font-medium transition-colors",
                dim === key
                  ? "bg-card text-foreground shadow-xs"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              {label}
            </button>
          ))}
        </div>
      </div>

      {rows.length === 0 ? (
        <p className="px-3.5 py-4 text-meta text-muted-foreground">
          No token usage recorded for this breakdown in the last {overview.days} days.
        </p>
      ) : (
        <table className="w-full border-collapse text-left text-meta">
          <thead>
            <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
              <th className="px-3.5 py-2 capitalize">{dim}</th>
              <th className="px-3.5 py-2 text-right">Runs</th>
              <th className="px-3.5 py-2 text-right">Gross in</th>
              <th className="px-3.5 py-2 text-right">Billable in</th>
              <th className="px-3.5 py-2 text-right">Cached</th>
              <th className="px-3.5 py-2 text-right">Cache hit</th>
              <th className="px-3.5 py-2 text-right">Avg billable/run</th>
              <th className="px-3.5 py-2 text-right">Out</th>
            </tr>
          </thead>
          <tbody className="divide-y tabular-nums">
            {rows.map((row) => (
              <tr key={row.key}>
                <td className="px-3.5 py-2 font-medium text-foreground">
                  {dim === "board" ? (
                    <Link to="/boards/$slug" params={{ slug: row.key }} className="ident">
                      {row.key}
                    </Link>
                  ) : (
                    <span className="ident">{row.key}</span>
                  )}
                </td>
                <td className="px-3.5 py-2 text-right">{row.runs}</td>
                <td className="px-3.5 py-2 text-right">{formatTokens(row.tokens_in)}</td>
                <td className="px-3.5 py-2 text-right font-medium text-foreground">
                  {formatTokens(row.billable_in)}
                </td>
                <td className="px-3.5 py-2 text-right text-muted-foreground">
                  {formatTokens(row.cached_tokens)}
                </td>
                <td className="px-3.5 py-2 text-right">{formatPct(row.cache_hit_rate)}</td>
                <td className="px-3.5 py-2 text-right text-muted-foreground">
                  {formatTokens(row.avg_billable_per_run)}
                </td>
                <td className="px-3.5 py-2 text-right text-muted-foreground">
                  {formatTokens(row.tokens_out)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

function HostHealthCard({
  host,
  device,
  skewedComponents,
}: {
  host: HostHealthSummary;
  device: DeviceEntry | undefined;
  skewedComponents: Set<string>;
}) {
  const res = host.resources;
  const net = host.network;

  const memPct =
    res?.mem_used_gib !== null &&
    res?.mem_used_gib !== undefined &&
    res.mem_total_gib !== null &&
    res.mem_total_gib !== undefined &&
    res.mem_total_gib > 0
      ? (res.mem_used_gib / res.mem_total_gib) * 100
      : null;

  const diskPct =
    res?.disk_used_gib !== null &&
    res?.disk_used_gib !== undefined &&
    res.disk_total_gib !== null &&
    res.disk_total_gib !== undefined &&
    res.disk_total_gib > 0
      ? (res.disk_used_gib / res.disk_total_gib) * 100
      : null;

  const totalUnits = device?.capacity?.weight_units ?? null;
  const freeUnits = device?.free_units ?? null;
  const activeUnits =
    totalUnits !== null && freeUnits !== null ? Math.max(0, totalUnits - freeUnits) : null;
  const loadPct =
    totalUnits !== null && totalUnits > 0 && activeUnits !== null
      ? Math.min(100, (activeUnits / totalUnits) * 100)
      : 0;

  const hwSummary = [
    host.os && host.arch ? `${host.os}/${host.arch}` : (host.os ?? host.arch),
    device?.capacity?.cpu_cores ? `${device.capacity.cpu_cores}c` : null,
    device?.capacity?.mem_gib
      ? `${device.capacity.mem_gib} GiB`
      : res?.mem_total_gib
        ? `${Math.round(res.mem_total_gib)} GiB`
        : null,
    host.uptime_secs !== null ? `up ${formatDuration(host.uptime_secs)}` : null,
  ].filter(Boolean);

  const versionEntries = Object.entries(host.versions);

  return (
    <article className="grid content-start gap-3 rounded-lg border bg-card p-4">
      <header className="flex flex-wrap items-start justify-between gap-2">
        <div className="grid gap-1">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="ident text-sm font-semibold text-foreground">{host.host}</h3>
            <DeviceStatusPill status={host.status} />
            {(host.device_role ?? device?.role) && (
              <Badge variant="outline" shape="tag">
                {host.device_role ?? device?.role}
              </Badge>
            )}
            {device?.drain && (
              <Badge variant="warn" shape="tag">
                draining
              </Badge>
            )}
          </div>
          <p className="text-meta text-muted-foreground">
            {hwSummary.length > 0 ? hwSummary.join(" · ") : "Host telemetry pending"}
          </p>
        </div>
        <span className="text-xs whitespace-nowrap text-muted-foreground">
          {host.reported_at ? (
            <>
              reported <RelTime at={host.reported_at} />
            </>
          ) : (
            "no report"
          )}
        </span>
      </header>

      {/* Connectivity & collector strip */}
      {(net || host.collectors.length > 0) && (
        <div className="flex flex-wrap items-center gap-1.5 text-xs">
          {net?.tailscale_online !== null && net?.tailscale_online !== undefined && (
            <Badge
              variant={
                !net.tailscale_online ? "bad" : net.tailscale_mode === "relay" ? "warn" : "ok"
              }
              shape="tag"
            >
              Tailscale:{" "}
              {!net.tailscale_online
                ? "offline"
                : `${net.tailscale_mode ?? "online"}${net.tailscale_latency_ms !== null ? ` ${net.tailscale_latency_ms.toFixed(0)}ms` : ""}`}
            </Badge>
          )}
          {net?.heartbeat_ok !== null && net?.heartbeat_ok !== undefined && (
            <Badge variant={net.heartbeat_ok ? "ok" : "bad"} shape="tag">
              {net.heartbeat_label ?? "Heartbeat"}: {net.heartbeat_ok ? "ok" : "fail"}
            </Badge>
          )}
          {host.collectors.map((c) => (
            <Badge key={c} variant="secondary" shape="tag" className="font-mono">
              {c}
            </Badge>
          ))}
        </div>
      )}

      {/* Resource bars */}
      {res && (
        <div className="grid gap-2">
          {(res.cpu_pct !== null || res.load_1m !== null) && (
            <div className="grid gap-1">
              <div className="flex items-center justify-between gap-2 text-xs tabular-nums">
                <span className="text-muted-foreground">
                  CPU
                  {res.slice_cpu_cap_pct !== null ? ` (cap ${res.slice_cpu_cap_pct}%)` : ""}
                </span>
                <span className="font-medium text-foreground">
                  {res.cpu_pct !== null ? `${res.cpu_pct.toFixed(1)}%` : "—"}
                  {res.load_1m !== null && (
                    <span className="ml-1.5 font-normal text-muted-foreground">
                      load {res.load_1m.toFixed(2)}
                      {res.load_5m !== null ? ` / ${res.load_5m.toFixed(2)}` : ""}
                    </span>
                  )}
                </span>
              </div>
              {res.cpu_pct !== null && (
                <MeterBar
                  percent={res.cpu_pct}
                  tone={res.cpu_pct >= 90 ? "blocked" : res.cpu_pct >= 75 ? "warn" : "doing"}
                  label={`${host.host} CPU ${res.cpu_pct.toFixed(1)}%`}
                />
              )}
            </div>
          )}

          {memPct !== null && res.mem_used_gib !== null && res.mem_total_gib !== null && (
            <div className="grid gap-1">
              <div className="flex items-center justify-between gap-2 text-xs tabular-nums">
                <span className="text-muted-foreground">Memory</span>
                <span className="font-medium text-foreground">
                  {res.mem_used_gib.toFixed(1)} / {res.mem_total_gib.toFixed(1)} GiB (
                  {memPct.toFixed(0)}%)
                </span>
              </div>
              <MeterBar
                percent={memPct}
                tone={memPct >= 90 ? "blocked" : memPct >= 80 ? "warn" : "ready"}
                label={`${host.host} memory ${memPct.toFixed(0)}%`}
              />
            </div>
          )}

          {diskPct !== null && res.disk_used_gib !== null && res.disk_total_gib !== null && (
            <div className="grid gap-1">
              <div className="flex items-center justify-between gap-2 text-xs tabular-nums">
                <span className="text-muted-foreground">Root disk</span>
                <span className="font-medium text-foreground">
                  {res.disk_used_gib.toFixed(1)} / {res.disk_total_gib.toFixed(1)} GiB
                  {res.disk_free_gib !== null && (
                    <span className="ml-1.5 font-normal text-muted-foreground">
                      ({res.disk_free_gib.toFixed(1)} GiB free
                      {res.disk_days_to_full !== null
                        ? ` · ~${Math.round(res.disk_days_to_full)}d`
                        : ""}
                      )
                    </span>
                  )}
                </span>
              </div>
              <MeterBar
                percent={diskPct}
                tone={diskPct >= 90 ? "blocked" : diskPct >= 80 ? "warn" : "ready"}
                label={`${host.host} root disk ${diskPct.toFixed(0)}%`}
              />
            </div>
          )}
        </div>
      )}

      {/* Fleet weighted runner capacity */}
      {totalUnits !== null && (
        <div className="grid gap-1">
          <div className="flex items-center justify-between text-xs text-muted-foreground tabular-nums">
            <span>Runner capacity</span>
            <span className="font-medium text-foreground">
              {activeUnits !== null
                ? `${activeUnits} / ${totalUnits} units`
                : `${totalUnits} units cap`}
              {freeUnits !== null ? ` (${freeUnits} free)` : ""}
            </span>
          </div>
          <MeterBar
            percent={loadPct}
            tone={loadPct >= 90 ? "blocked" : loadPct >= 70 ? "warn" : "doing"}
            label={`${host.host} weighted load ${Math.round(loadPct)}%`}
          />
        </div>
      )}

      <HostSparkline samples={host.samples} host={host.host} />

      {/* Component versions & Fleet metadata */}
      {(versionEntries.length > 0 || (device?.taints.length ?? 0) > 0) && (
        <div className="flex flex-wrap items-center gap-1.5 border-t pt-2.5 text-xs">
          {versionEntries.map(([comp, ver]) => (
            <Badge
              key={comp}
              variant={skewedComponents.has(comp) ? "warn" : "outline"}
              shape="tag"
              className="font-mono"
            >
              {comp}:{ver}
            </Badge>
          ))}
          {device?.taints.map((t) => {
            const label = `${t.value ? `${t.key}=${t.value}` : t.key}:${t.effect}`;
            return (
              <Badge
                key={label}
                variant={t.effect === "no_schedule" ? "bad" : "warn"}
                shape="tag"
                className="font-mono"
              >
                {label}
              </Badge>
            );
          })}
        </div>
      )}

      {device &&
        (device.runners.length > 0 ||
          device.repos.length > 0 ||
          device.capabilities.length > 0) && (
          <details className="text-xs text-muted-foreground">
            <summary className="cursor-pointer hover:text-foreground">
              Runners ({device.runners.length}) · Repos ({device.repos.length}) · Capabilities
            </summary>
            <div className="mt-1.5 grid gap-1.5 rounded-sm bg-muted p-2.5">
              {device.runners.length > 0 && (
                <p className="font-mono">Runners: {device.runners.join(", ")}</p>
              )}
              {device.repos.length > 0 && (
                <p className="font-mono">Repos: {device.repos.join(", ")}</p>
              )}
              {device.capabilities.length > 0 && (
                <p className="font-mono">Capabilities: {device.capabilities.join(", ")}</p>
              )}
            </div>
          </details>
        )}
    </article>
  );
}

/**
 * Unified System Health screen (`/health`), folding in `/fleet`:
 * global posture pulse bar, hosts/resources, services/containers, token & cache analytics + quota
 * pacing, autonomous AI factory & scheduler policy/autotune, storage/backups, and security/CI.
 */
export function HealthScreen() {
  useTitle("Health");
  const search = healthRoute.useSearch();
  const navigate = useNavigate({ from: "/health" });
  const days = parseDays(search.days);

  const queryClient = useQueryClient();
  const overviewQuery = useQuery(queries.healthOverview(days));
  const devicesQuery = useQuery(queries.devices());
  const bucketsQuery = useQuery(queries.usageBuckets());
  const statsQuery = useQuery(queries.schedulerStats(days));
  const policyQuery = useQuery(queries.schedulerPolicy());
  const evaluationsQuery = useQuery(queries.schedulerEvaluations(20));

  const refreshMutation = useMutation({
    mutationFn: fetching(() => api.refreshHealth()),
    onSuccess: () => {
      void invalidateTags(queryClient, [tags.health, tags.schedules, tags.devices, tags.usage]);
    },
  });

  const runEvaluation = useMutation({
    mutationFn: fetching(() => api.runSchedulerEvaluation()),
    onSuccess: () => {
      void invalidateTags(queryClient, [tags.scheduler, tags.health]);
    },
  });

  const now = useNow(30_000);
  const [serviceHostFilter, setServiceHostFilter] = useState("");
  const [serviceProblemsOnly, setServiceProblemsOnly] = useState(false);

  const windowOptions = WINDOW_DAYS.includes(days as (typeof WINDOW_DAYS)[number])
    ? [...WINDOW_DAYS]
    : [...WINDOW_DAYS, days].sort((a, b) => a - b);

  const overview = overviewQuery.data;
  const devices = useMemo(
    () => devicesQuery.data ?? overview?.devices ?? [],
    [devicesQuery.data, overview?.devices],
  );
  const buckets = bucketsQuery.data ?? overview?.usage_buckets ?? [];

  const deviceMap = useMemo(() => new Map(devices.map((d) => [d.name, d])), [devices]);
  const skewedComponents = useMemo(
    () => new Set((overview?.version_skews ?? []).map((v) => v.component)),
    [overview?.version_skews],
  );

  // Combine containers and services across all hosts, unhealthy first.
  const serviceRows = useMemo(() => {
    const out: {
      key: string;
      host: string;
      name: string;
      kind: string;
      status: string;
      detail: string;
      extra: string;
      rank: number;
    }[] = [];
    for (const h of overview?.hosts ?? []) {
      for (const c of h.containers) {
        const eff = c.health && c.health !== "none" ? c.health : c.state;
        const tone = serviceTone(eff);
        out.push({
          key: `${h.host}:docker:${c.name}`,
          host: h.host,
          name: c.name,
          kind: "docker",
          status: eff,
          detail: c.status_text ?? c.image ?? c.state,
          extra: c.restarts !== null ? `${c.restarts} restarts` : "—",
          rank: tone.rank,
        });
      }
      for (const s of h.services) {
        const tone = serviceTone(s.state);
        const extras = [
          s.memory_mib !== null ? `${s.memory_mib.toFixed(0)} MiB` : null,
          s.restarts !== null && s.restarts > 0 ? `${s.restarts} restarts` : null,
          s.pid !== null ? `pid ${s.pid}` : null,
        ].filter(Boolean);
        out.push({
          key: `${h.host}:${s.kind}:${s.name}`,
          host: h.host,
          name: s.name,
          kind: s.kind,
          status: s.state,
          detail: s.detail ?? s.state,
          extra: extras.length > 0 ? extras.join(" · ") : "—",
          rank: tone.rank,
        });
      }
    }
    return out
      .filter(
        (r) =>
          (!serviceHostFilter || r.host === serviceHostFilter) &&
          (!serviceProblemsOnly || r.rank < 2),
      )
      .sort(
        (a, b) => a.rank - b.rank || a.host.localeCompare(b.host) || a.name.localeCompare(b.name),
      );
  }, [overview?.hosts, serviceHostFilter, serviceProblemsOnly]);

  const hostNamesInServices = useMemo(
    () => [...new Set((overview?.hosts ?? []).map((h) => h.host))].sort(),
    [overview?.hosts],
  );

  // Devices not represented in `overview.hosts` yet (fallback to Fleet DeviceCard).
  const unmergedDevices = useMemo(() => {
    const known = new Set((overview?.hosts ?? []).map((h) => h.host));
    return devices.filter((d) => !known.has(d.name));
  }, [devices, overview?.hosts]);

  const firingAlerts = useMemo(
    () =>
      (overview?.alerts ?? [])
        .filter((a) => !a.is_watchdog)
        .sort((a, b) => {
          const rank = (s: AlertSeverity) => (s === "critical" ? 0 : s === "warning" ? 1 : 2);
          return rank(a.severity) - rank(b.severity) || a.name.localeCompare(b.name);
        }),
    [overview?.alerts],
  );

  const watchdogAlerts = useMemo(
    () => (overview?.alerts ?? []).filter((a) => a.is_watchdog),
    [overview?.alerts],
  );

  const posture = postureMeta(overview?.posture ?? "unknown");

  return (
    <div className="grid gap-7">
      <PageHeader
        title="System Health"
        sub={
          overview ? (
            <>
              Updated <RelTime at={overview.generated_at} /> · server{" "}
              <span className="ident">{overview.server.commit}</span> (schema v
              {overview.server.schema_version})
            </>
          ) : (
            "Cross-machine fleet health, services, token efficiency, storage, and security."
          )
        }
        aside={
          <div className="flex flex-wrap items-center gap-2">
            <FilterSelect
              label="Telemetry window"
              value={String(days)}
              onChange={(value) =>
                void navigate({
                  search: value === "7" ? {} : { days: value },
                  replace: true,
                })
              }
            >
              {windowOptions.map((d) => (
                <NativeSelectOption key={d} value={String(d)}>
                  Last {d} {d === 1 ? "day" : "days"}
                </NativeSelectOption>
              ))}
            </FilterSelect>
            <Button
              variant="outline"
              size="sm"
              onClick={() => refreshMutation.mutate()}
              disabled={refreshMutation.isPending}
              className="h-[34px] gap-1.5"
            >
              <Icon
                name="refresh"
                className={cn("size-3.5", refreshMutation.isPending && "animate-spin")}
              />
              {refreshMutation.isPending ? "Refreshing…" : "Refresh now"}
            </Button>
          </div>
        }
      >
        {overview && (
          <span
            className={cn(
              "inline-flex h-6 items-center gap-1.5 rounded-full border px-2.5 text-xs font-semibold tracking-wide uppercase",
              posture.pill,
            )}
          >
            <span className={cn("size-2 rounded-full", posture.dot)} aria-hidden="true" />
            {posture.label}
          </span>
        )}
      </PageHeader>

      {refreshMutation.error && (
        <ErrorNotice error={refreshMutation.error} retry={() => refreshMutation.mutate()} compact />
      )}
      {refreshMutation.data && (
        <div
          className="flex flex-wrap items-center justify-between gap-2 rounded-md border bg-muted px-3.5 py-2 text-meta text-muted-foreground"
          role="status"
        >
          <span>
            On-demand refresh requested:{" "}
            <strong className="font-medium text-foreground">
              {refreshMutation.data.triggered.length} triggered
            </strong>
            {refreshMutation.data.triggered.length > 0 &&
              ` (${refreshMutation.data.triggered.join(", ")})`}
            {refreshMutation.data.failed.length > 0 &&
              ` · ${refreshMutation.data.failed.length} unreachable`}
          </span>
        </div>
      )}

      {/* Incident Pulse Chips + Quick Section Jump Bar */}
      {overview && (
        <div className="grid gap-3">
          <div className="flex flex-wrap items-center gap-2">
            <a
              href="#health-infra"
              className={cn(
                "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs font-medium hover:no-underline",
                overview.incidents.firing_alerts > 0
                  ? "border-status-blocked/40 bg-status-blocked-soft text-status-blocked"
                  : "border-status-ready/35 bg-status-ready-soft text-status-ready",
              )}
            >
              <span className="font-semibold tabular-nums">{overview.incidents.firing_alerts}</span>
              <span>firing {overview.incidents.firing_alerts === 1 ? "alert" : "alerts"}</span>
            </a>

            {overview.incidents.watchdog_ok !== null && (
              <a
                href="#health-infra"
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs font-medium hover:no-underline",
                  overview.incidents.watchdog_ok
                    ? "border-status-ready/35 bg-status-ready-soft text-status-ready"
                    : "border-amber/40 bg-amber-soft text-amber",
                )}
              >
                Watchdog {overview.incidents.watchdog_ok ? "alive" : "missing"}
              </a>
            )}

            <Link
              to="/schedules"
              search={overview.incidents.failing_jobs > 0 ? { status: "failing" } : {}}
              className={cn(
                "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs font-medium hover:no-underline",
                overview.incidents.failing_jobs > 0
                  ? "border-status-blocked/40 bg-status-blocked-soft text-status-blocked"
                  : "border-border bg-card text-subtle-foreground hover:bg-muted",
              )}
            >
              <span className="font-semibold tabular-nums">{overview.incidents.failing_jobs}</span>
              <span>failing jobs</span>
            </Link>

            <Link
              to="/schedules"
              search={overview.incidents.overdue_jobs > 0 ? { status: "overdue" } : {}}
              className={cn(
                "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs font-medium hover:no-underline",
                overview.incidents.overdue_jobs > 0
                  ? "border-amber/40 bg-amber-soft text-amber"
                  : "border-border bg-card text-subtle-foreground hover:bg-muted",
              )}
            >
              <span className="font-semibold tabular-nums">{overview.incidents.overdue_jobs}</span>
              <span>overdue jobs</span>
            </Link>

            <a
              href="#health-hosts"
              className={cn(
                "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs font-medium hover:no-underline",
                overview.incidents.stale_hosts > 0
                  ? "border-amber/40 bg-amber-soft text-amber"
                  : "border-border bg-card text-subtle-foreground hover:bg-muted",
              )}
            >
              <span className="font-semibold tabular-nums">{overview.incidents.stale_hosts}</span>
              <span>stale hosts</span>
            </a>

            {overview.incidents.version_skews > 0 && (
              <a
                href="#health-hosts"
                className="inline-flex items-center gap-1.5 rounded-full border border-amber/40 bg-amber-soft px-2.5 py-1 text-xs font-medium text-amber hover:no-underline"
              >
                <span className="font-semibold tabular-nums">
                  {overview.incidents.version_skews}
                </span>
                <span>version skew</span>
              </a>
            )}

            <Link
              to="/asks"
              className={cn(
                "inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs font-medium hover:no-underline",
                overview.incidents.open_asks > 0
                  ? "border-amber/40 bg-amber-soft text-amber"
                  : "border-border bg-card text-subtle-foreground hover:bg-muted",
              )}
            >
              <span className="font-semibold tabular-nums">{overview.incidents.open_asks}</span>
              <span>open asks</span>
            </Link>

            {overview.incidents.stale_leases > 0 && (
              <a
                href="#health-factory"
                className="inline-flex items-center gap-1.5 rounded-full border border-amber/40 bg-amber-soft px-2.5 py-1 text-xs font-medium text-amber hover:no-underline"
              >
                <span className="font-semibold tabular-nums">
                  {overview.incidents.stale_leases}
                </span>
                <span>stale leases</span>
              </a>
            )}
          </div>

          <nav
            aria-label="Health sections"
            className="flex flex-wrap items-center gap-1.5 border-b pb-3 text-xs"
          >
            {[
              ["#health-hosts", "Hosts & Fleet"],
              ["#health-services", "Services & Containers"],
              ["#health-tokens", "Tokens & Quotas"],
              ["#health-factory", "AI Factory & Scheduler"],
              ["#health-storage", "Storage & Backups"],
              ["#health-infra", "Security, Alerts & CI"],
            ].map(([href, label]) => (
              <a
                key={href}
                href={href}
                className="rounded-md px-2 py-1 font-medium text-muted-foreground hover:bg-muted hover:text-foreground hover:no-underline"
              >
                {label}
              </a>
            ))}
          </nav>
        </div>
      )}

      <QueryFrame query={overviewQuery} label="Loading system health overview">
        {(data) => (
          <div className="grid gap-8">
            {/* Pillar 1: Hosts, Resources, Connectivity & Fleet Nodes */}
            <section
              id="health-hosts"
              aria-labelledby="health-hosts-heading"
              className="grid gap-3.5"
            >
              <SectionTitle
                id="health-hosts-heading"
                count={data.hosts.length + unmergedDevices.length}
              >
                Hosts &amp; fleet nodes
              </SectionTitle>

              {data.version_skews.length > 0 && (
                <div className="grid gap-1.5 rounded-lg border border-warn-border bg-warn-soft px-3.5 py-2.5 text-meta text-warn">
                  <p className="font-semibold">Version skew detected</p>
                  <ul className="grid gap-1 text-xs">
                    {data.version_skews.map((skew) => (
                      <li key={skew.component} className="flex flex-wrap items-center gap-1.5">
                        <span className="ident font-semibold">{skew.component}</span>
                        <span>expected</span>
                        <code className="rounded-xs bg-card/70 px-1 py-px">{skew.expected}</code>
                        <span>— behind:</span>
                        {skew.mismatched.map((m) => (
                          <Badge
                            key={`${skew.component}:${m.host}`}
                            variant="warn"
                            shape="tag"
                            className="font-mono"
                          >
                            {m.host}={m.version}
                          </Badge>
                        ))}
                      </li>
                    ))}
                  </ul>
                </div>
              )}

              {data.hosts.length === 0 && unmergedDevices.length === 0 ? (
                <Empty icon="health" title="No host telemetry reported yet">
                  Run <code>board report-host</code> or enable <code>[telemetry]</code> in{" "}
                  <code>board watch</code> to stream host resources and services.
                </Empty>
              ) : (
                <div className="grid grid-cols-1 gap-3.5 lg:grid-cols-2">
                  {data.hosts.map((host) => (
                    <HostHealthCard
                      key={host.host}
                      host={host}
                      device={deviceMap.get(host.host)}
                      skewedComponents={skewedComponents}
                    />
                  ))}
                  {unmergedDevices.map((device) => (
                    <DeviceCard key={device.name} device={device} />
                  ))}
                </div>
              )}
            </section>

            {/* Pillar 2: Services & Containers Matrix */}
            <section
              id="health-services"
              aria-labelledby="health-services-heading"
              className="grid gap-3"
            >
              <div className="flex flex-wrap items-center justify-between gap-2">
                <SectionTitle id="health-services-heading" count={serviceRows.length}>
                  Services &amp; containers
                </SectionTitle>
                <div className="flex flex-wrap items-center gap-2">
                  <FilterSelect
                    label="Host"
                    value={serviceHostFilter}
                    onChange={setServiceHostFilter}
                  >
                    <NativeSelectOption value="">All hosts</NativeSelectOption>
                    {hostNamesInServices.map((h) => (
                      <NativeSelectOption key={h} value={h}>
                        {h}
                      </NativeSelectOption>
                    ))}
                  </FilterSelect>
                  <FilterToggle checked={serviceProblemsOnly} onChange={setServiceProblemsOnly}>
                    Problems only
                  </FilterToggle>
                </div>
              </div>

              {serviceRows.length === 0 ? (
                <Empty icon="fleet" title="No matching services or containers">
                  {serviceProblemsOnly
                    ? "Every reported container and daemon is running healthy."
                    : "Host collectors will populate Docker containers, systemd units, and launchd daemons here."}
                </Empty>
              ) : (
                <div className="overflow-x-auto rounded-lg border bg-card">
                  <table className="w-full border-collapse text-left text-meta">
                    <thead>
                      <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
                        <th className="px-3.5 py-2">Status</th>
                        <th className="px-3.5 py-2">Service / Container</th>
                        <th className="px-3.5 py-2">Host</th>
                        <th className="px-3.5 py-2">Kind</th>
                        <th className="px-3.5 py-2">Detail</th>
                        <th className="px-3.5 py-2 text-right">Footprint</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y">
                      {serviceRows.map((row) => {
                        const tone = serviceTone(row.status);
                        return (
                          <tr key={row.key}>
                            <td className="px-3.5 py-2 whitespace-nowrap">
                              <Badge variant={tone.variant} shape="tag">
                                {tone.label}
                              </Badge>
                            </td>
                            <td className="px-3.5 py-2 font-mono text-xs font-medium text-foreground">
                              {row.name}
                            </td>
                            <td className="px-3.5 py-2 font-mono text-xs text-muted-foreground">
                              {row.host}
                            </td>
                            <td className="px-3.5 py-2">
                              <Badge variant="secondary" shape="tag" className="font-mono">
                                {row.kind}
                              </Badge>
                            </td>
                            <td className="max-w-sm truncate px-3.5 py-2 text-muted-foreground">
                              {row.detail}
                            </td>
                            <td className="px-3.5 py-2 text-right whitespace-nowrap text-muted-foreground tabular-nums">
                              {row.extra}
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
              )}
            </section>

            {/* Pillar 3: Token Usage, Cache Efficiency & Quota Pacing */}
            <section
              id="health-tokens"
              aria-labelledby="health-tokens-heading"
              className="grid gap-4"
            >
              <SectionTitle id="health-tokens-heading">
                Token usage, cache efficiency &amp; quota pacing
              </SectionTitle>

              {buckets.length > 0 && (
                <div className="grid grid-cols-1 gap-3.5 md:grid-cols-2">
                  {buckets.map((bucket) => (
                    <QuotaBucketCard
                      key={`${bucket.provider}:${bucket.account}`}
                      bucket={bucket}
                      now={now}
                    />
                  ))}
                </div>
              )}

              <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
                <MetricTile
                  label={`Gross input (${data.tokens.days}d)`}
                  value={formatTokens(data.tokens.tokens_in)}
                  sub={`${data.tokens.runs_with_tokens} metered runs`}
                />
                <MetricTile
                  label="Billable input"
                  value={formatTokens(data.tokens.billable_in)}
                  sub={`cached ${formatTokens(data.tokens.cached_tokens)}`}
                />
                <MetricTile
                  label="Prompt cache hit"
                  value={formatPct(data.tokens.cache_hit_rate)}
                  sub="higher lowers effective cost"
                />
                <MetricTile
                  label="Output tokens"
                  value={formatTokens(data.tokens.tokens_out)}
                  sub={
                    data.tokens.reasoning_tokens > 0
                      ? `reasoning ${formatTokens(data.tokens.reasoning_tokens)}`
                      : undefined
                  }
                />
              </div>

              <div className="rounded-lg border bg-card p-4">
                <TokenTrendChart overview={data} />
              </div>

              <TokenBreakdownTable overview={data} />
            </section>

            {/* Pillar 4: Autonomous AI Factory, Deploy Trains, Leases & Scheduler */}
            <section
              id="health-factory"
              aria-labelledby="health-factory-heading"
              className="grid gap-4"
            >
              <SectionTitle id="health-factory-heading">
                Autonomous AI factory &amp; scheduler
              </SectionTitle>

              <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
                <MetricTile
                  label="Board server"
                  value={`v${data.server.schema_version}`}
                  sub={`up ${formatDuration(data.server.uptime_secs)} · ${formatBytes(data.server.db_bytes)} DB / ${formatBytes(data.server.wal_bytes)} WAL`}
                />
                <MetricTile
                  label="Active runs"
                  value={String(data.factory.active_runs)}
                  sub={`ready ${data.factory.queue.ready_auto} auto / ${data.factory.queue.ready_manual} manual · review ${data.factory.queue.review}`}
                />
                <MetricTile
                  label={`Runs (${data.days}d)`}
                  value={String(data.factory.runs_total)}
                  sub={`${data.factory.runs_succeeded} ok · ${data.factory.runs_failed} fail · ${data.factory.runs_orphaned} orph`}
                />
                <MetricTile
                  label="First-pass merge"
                  value={formatPct(data.factory.first_pass_merge_rate)}
                  sub={
                    data.factory.avg_review_rounds !== null
                      ? `avg ${data.factory.avg_review_rounds.toFixed(2)} review rounds · ${data.factory.open_feedback} feedback`
                      : `${data.factory.open_feedback} open feedback`
                  }
                />
              </div>

              {/* Deploy trains */}
              {data.factory.deploy_trains.length > 0 && (
                <div className="overflow-x-auto rounded-lg border bg-card">
                  <div className="border-b px-3.5 py-2.5">
                    <h3 className="text-xs font-semibold text-subtle-foreground">Deploy trains</h3>
                  </div>
                  <table className="w-full border-collapse text-left text-meta">
                    <thead>
                      <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
                        <th className="px-3.5 py-2">Board</th>
                        <th className="px-3.5 py-2">Repo</th>
                        <th className="px-3.5 py-2">Pending</th>
                        <th className="px-3.5 py-2">Flush / Lock</th>
                        <th className="px-3.5 py-2">Last deploy</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y">
                      {data.factory.deploy_trains.map((dt) => (
                        <tr key={`${dt.board}:${dt.repo ?? "none"}`}>
                          <td className="px-3.5 py-2">
                            <Link to="/boards/$slug" params={{ slug: dt.board }} className="ident">
                              {dt.board}
                            </Link>
                          </td>
                          <td className="px-3.5 py-2 font-mono text-xs text-muted-foreground">
                            {dt.repo ?? "—"}
                          </td>
                          <td className="px-3.5 py-2">
                            {dt.pending_tasks.length > 0 ? (
                              <div className="flex flex-wrap gap-1">
                                {dt.pending_tasks.map((tid) => (
                                  <Link
                                    key={tid}
                                    to="/tasks/$id"
                                    params={{ id: tid }}
                                    className="ident text-xs"
                                  >
                                    {tid}
                                  </Link>
                                ))}
                              </div>
                            ) : (
                              <span className="text-muted-foreground">0</span>
                            )}
                          </td>
                          <td className="px-3.5 py-2 text-xs text-muted-foreground">
                            {dt.active_lock_holder ? (
                              <Badge variant="doing" shape="tag" className="font-mono">
                                lock: {dt.active_lock_holder}
                              </Badge>
                            ) : dt.flush_at ? (
                              <>flushes {formatUntil(dt.flush_at, now)}</>
                            ) : (
                              "idle"
                            )}
                          </td>
                          <td className="px-3.5 py-2 text-xs text-muted-foreground">
                            {dt.last_deploy_at ? (
                              <>
                                <RelTime at={dt.last_deploy_at} />
                                {dt.last_deploy_sha ? ` (${dt.last_deploy_sha.slice(0, 7)})` : ""}
                              </>
                            ) : (
                              "—"
                            )}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}

              {/* Leases & Goal Cooldowns */}
              <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
                <div className="overflow-x-auto rounded-lg border bg-card">
                  <div className="flex items-center justify-between border-b px-3.5 py-2.5">
                    <h3 className="text-xs font-semibold text-subtle-foreground">
                      Lease hygiene ({data.factory.leases.active_count} active)
                    </h3>
                    {data.factory.leases.stale_holder_count > 0 ? (
                      <Badge variant="bad" shape="tag">
                        {data.factory.leases.stale_holder_count} stale
                      </Badge>
                    ) : (
                      <Badge variant="ok" shape="tag">
                        clean
                      </Badge>
                    )}
                  </div>
                  {data.factory.leases.items.length === 0 ? (
                    <p className="px-3.5 py-4 text-meta text-muted-foreground">
                      No active path leases.
                    </p>
                  ) : (
                    <table className="w-full border-collapse text-left text-xs">
                      <thead>
                        <tr className="border-b bg-muted font-medium text-muted-foreground">
                          <th className="px-3 py-1.5">Glob</th>
                          <th className="px-3 py-1.5">Task</th>
                          <th className="px-3 py-1.5">Expires</th>
                        </tr>
                      </thead>
                      <tbody className="divide-y">
                        {data.factory.leases.items.map((lease) => (
                          <tr key={lease.id}>
                            <td className="px-3 py-1.5 font-mono text-foreground">
                              {lease.glob}
                              {lease.stale_holder && (
                                <Badge variant="bad" shape="tag" className="ml-1.5">
                                  stale holder
                                </Badge>
                              )}
                            </td>
                            <td className="px-3 py-1.5 font-mono text-muted-foreground">
                              {lease.task ? (
                                <Link to="/tasks/$id" params={{ id: lease.task }}>
                                  {lease.task}
                                </Link>
                              ) : (
                                lease.agent
                              )}
                            </td>
                            <td className="px-3 py-1.5 whitespace-nowrap text-muted-foreground">
                              <RelTime at={lease.expires_at} />
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  )}
                </div>

                <div className="overflow-x-auto rounded-lg border bg-card">
                  <div className="border-b px-3.5 py-2.5">
                    <h3 className="text-xs font-semibold text-subtle-foreground">
                      Board goal-review cooldowns
                    </h3>
                  </div>
                  {data.factory.goal_cooldowns.length === 0 ? (
                    <p className="px-3.5 py-4 text-meta text-muted-foreground">
                      No active boards with scheduled goal reviews.
                    </p>
                  ) : (
                    <table className="w-full border-collapse text-left text-xs">
                      <thead>
                        <tr className="border-b bg-muted font-medium text-muted-foreground">
                          <th className="px-3 py-1.5">Board</th>
                          <th className="px-3 py-1.5">Cadence</th>
                          <th className="px-3 py-1.5">Next eligible</th>
                        </tr>
                      </thead>
                      <tbody className="divide-y">
                        {data.factory.goal_cooldowns.map((g) => (
                          <tr key={g.board}>
                            <td className="px-3 py-1.5">
                              <Link to="/boards/$slug" params={{ slug: g.board }} className="ident">
                                {g.board}
                              </Link>
                            </td>
                            <td className="px-3 py-1.5 text-muted-foreground tabular-nums">
                              {g.every_secs ? `every ${formatDuration(g.every_secs)}` : "manual"}
                            </td>
                            <td className="px-3 py-1.5 whitespace-nowrap text-muted-foreground">
                              {g.cooldown_remaining_secs && g.cooldown_remaining_secs > 0 ? (
                                <>in {formatDuration(g.cooldown_remaining_secs)}</>
                              ) : (
                                <Badge variant="ok" shape="tag">
                                  eligible now
                                </Badge>
                              )}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  )}
                </div>
              </div>

              {/* Folded-in Scheduler Policy, Telemetry & Autotune */}
              {policyQuery.error && (
                <ErrorNotice
                  error={policyQuery.error}
                  retry={() => void policyQuery.refetch()}
                  compact
                />
              )}
              {policyQuery.data && <PolicyCard policy={policyQuery.data} />}

              <QueryFrame query={statsQuery} label="Loading scheduler telemetry">
                {(stats) => <SchedulerTelemetryView stats={stats} />}
              </QueryFrame>

              <div className="grid gap-3">
                <h3 className="text-xs font-semibold text-subtle-foreground">
                  Automated evaluation &amp; self-tuning
                </h3>
                <QueryFrame query={evaluationsQuery} label="Loading scheduler evaluations">
                  {(evalData) => (
                    <SchedulerAutotuneView
                      data={evalData}
                      running={runEvaluation.isPending}
                      runError={runEvaluation.error}
                      onRun={() => runEvaluation.mutate()}
                    />
                  )}
                </QueryFrame>
              </div>
            </section>

            {/* Pillar 5: Storage Quotas & Backup Freshness */}
            <section
              id="health-storage"
              aria-labelledby="health-storage-heading"
              className="grid gap-4"
            >
              <SectionTitle id="health-storage-heading">
                Storage quotas &amp; backup freshness
              </SectionTitle>
              <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
                <div className="grid content-start gap-3 rounded-lg border bg-card p-4">
                  <h3 className="text-xs font-semibold text-subtle-foreground">
                    Storage allocations
                  </h3>
                  {data.storage.length === 0 ? (
                    <p className="text-meta text-muted-foreground">
                      No storage quota metrics reported yet.
                    </p>
                  ) : (
                    <div className="grid gap-3">
                      {data.storage.map((s) => {
                        const pct = s.quota_gib > 0 ? (s.used_gib / s.quota_gib) * 100 : 0;
                        const warnAt = s.warn_pct ?? 80;
                        return (
                          <div key={`${s.host}:${s.name}`} className="grid gap-1">
                            <div className="flex flex-wrap items-center justify-between gap-2 text-xs tabular-nums">
                              <span className="flex items-center gap-1.5 font-medium text-foreground">
                                <span className="ident">{s.name}</span>
                                <span className="text-muted-foreground">({s.host})</span>
                              </span>
                              <span className="text-foreground">
                                {s.used_gib.toFixed(1)} / {s.quota_gib.toFixed(1)} GiB (
                                {pct.toFixed(1)}%)
                              </span>
                            </div>
                            <MeterBar
                              percent={pct}
                              tone={pct >= 92 ? "blocked" : pct >= warnAt ? "warn" : "ready"}
                              markerPercent={warnAt}
                              label={`${s.name} storage ${pct.toFixed(1)}%`}
                            />
                            {s.detail && (
                              <p className="text-2xs text-muted-foreground">{s.detail}</p>
                            )}
                          </div>
                        );
                      })}
                    </div>
                  )}
                </div>

                <div className="overflow-x-auto rounded-lg border bg-card">
                  <div className="border-b px-3.5 py-2.5">
                    <h3 className="text-xs font-semibold text-subtle-foreground">
                      Backup freshness
                    </h3>
                  </div>
                  {data.backups.length === 0 ? (
                    <p className="px-3.5 py-4 text-meta text-muted-foreground">
                      No backup targets reported yet.
                    </p>
                  ) : (
                    <table className="w-full border-collapse text-left text-meta">
                      <thead>
                        <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
                          <th className="px-3.5 py-2">State</th>
                          <th className="px-3.5 py-2">Target</th>
                          <th className="px-3.5 py-2">Host</th>
                          <th className="px-3.5 py-2">Last success</th>
                        </tr>
                      </thead>
                      <tbody className="divide-y">
                        {data.backups.map((b) => (
                          <tr key={`${b.host}:${b.target}`}>
                            <td className="px-3.5 py-2">
                              <Badge variant={backupVariant(b.state)} shape="tag">
                                {b.state}
                              </Badge>
                            </td>
                            <td className="px-3.5 py-2 font-medium text-foreground">
                              {b.target}
                              {b.detail && (
                                <p className="text-xs font-normal text-muted-foreground">
                                  {b.detail}
                                </p>
                              )}
                            </td>
                            <td className="px-3.5 py-2 font-mono text-xs text-muted-foreground">
                              {b.host}
                            </td>
                            <td className="px-3.5 py-2 text-xs whitespace-nowrap text-muted-foreground">
                              {b.last_success_at ? <RelTime at={b.last_success_at} /> : "never"}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  )}
                </div>
              </div>
            </section>

            {/* Pillar 6: Security, Alerts, Certs, Vault & CI */}
            <section
              id="health-infra"
              aria-labelledby="health-infra-heading"
              className="grid gap-4"
            >
              <SectionTitle id="health-infra-heading">
                Security, alerts, certificates &amp; CI
              </SectionTitle>

              {/* External Alerts (Watchdog separated) */}
              <div className="overflow-x-auto rounded-lg border bg-card">
                <div className="flex flex-wrap items-center justify-between gap-2 border-b px-3.5 py-2.5">
                  <h3 className="text-xs font-semibold text-subtle-foreground">
                    External alerts ({firingAlerts.length} firing)
                  </h3>
                  {watchdogAlerts.length > 0 && (
                    <Badge variant="ok" shape="tag">
                      Alert pipeline alive (Watchdog active)
                    </Badge>
                  )}
                </div>
                {firingAlerts.length === 0 ? (
                  <p className="px-3.5 py-4 text-meta text-muted-foreground">
                    No active alerts firing.
                  </p>
                ) : (
                  <table className="w-full border-collapse text-left text-meta">
                    <thead>
                      <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
                        <th className="px-3.5 py-2">Severity</th>
                        <th className="px-3.5 py-2">Alert</th>
                        <th className="px-3.5 py-2">Host</th>
                        <th className="px-3.5 py-2">Since</th>
                        <th className="px-3.5 py-2">Summary</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y">
                      {firingAlerts.map((a) => (
                        <tr key={`${a.host_reporter}:${a.name}:${a.host ?? ""}`}>
                          <td className="px-3.5 py-2">
                            <Badge variant={severityVariant(a.severity)} shape="tag">
                              {a.severity}
                            </Badge>
                          </td>
                          <td className="px-3.5 py-2 font-mono text-xs font-medium text-foreground">
                            {a.name}
                          </td>
                          <td className="px-3.5 py-2 font-mono text-xs text-muted-foreground">
                            {a.host ?? a.host_reporter}
                          </td>
                          <td className="px-3.5 py-2 text-xs whitespace-nowrap text-muted-foreground">
                            {a.active_since ? <RelTime at={a.active_since} /> : "—"}
                          </td>
                          <td className="px-3.5 py-2 text-muted-foreground">{a.summary}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                )}
              </div>

              {/* Vault & CI cards */}
              {(data.vault.length > 0 || data.ci.length > 0) && (
                <div className="grid grid-cols-1 gap-4 md:grid-cols-2">
                  {data.vault.map((v) => (
                    <article
                      key={v.host}
                      className="grid content-start gap-2.5 rounded-lg border bg-card p-4"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <h3 className="text-xs font-semibold text-subtle-foreground">
                          Vault ({v.host})
                        </h3>
                        <Badge variant={v.reachable && !v.sealed ? "ok" : "bad"} shape="tag">
                          {!v.reachable ? "unreachable" : v.sealed ? "sealed" : "unsealed"}
                        </Badge>
                      </div>
                      <div className="flex flex-wrap gap-2 text-xs text-muted-foreground">
                        {v.audit_ok !== null && <span>Audit: {v.audit_ok ? "ok" : "error"}</span>}
                        {v.agents_running !== null && (
                          <span>· {v.agents_running} agents running</span>
                        )}
                        {v.ca_valid_until && (
                          <span>
                            · CA valid until <AbsTime at={v.ca_valid_until} />
                          </span>
                        )}
                      </div>
                      {v.detail && <p className="text-meta text-muted-foreground">{v.detail}</p>}
                    </article>
                  ))}

                  {data.ci.map((c) => (
                    <article
                      key={c.host}
                      className="grid content-start gap-2.5 rounded-lg border bg-card p-4"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <h3 className="text-xs font-semibold text-subtle-foreground">
                          CI &amp; Runners ({c.host})
                        </h3>
                        <Badge
                          variant={
                            c.poller_fresh === false || (c.webhook_errors ?? 0) > 0 ? "warn" : "ok"
                          }
                          shape="tag"
                        >
                          {c.poller_fresh === false ? "poller stale" : "healthy"}
                        </Badge>
                      </div>
                      <div className="flex flex-wrap gap-2 text-xs text-muted-foreground tabular-nums">
                        {c.runners_online !== null && (
                          <span>
                            Runners: {c.runners_busy ?? 0} busy / {c.runners_online} online
                          </span>
                        )}
                        {c.webhook_errors !== null && (
                          <span>· {c.webhook_errors} webhook errors</span>
                        )}
                        {c.last_poll_at && (
                          <span>
                            · polled <RelTime at={c.last_poll_at} />
                          </span>
                        )}
                      </div>
                      {c.detail && <p className="text-meta text-muted-foreground">{c.detail}</p>}
                    </article>
                  ))}
                </div>
              )}

              {/* Certificates, Expiries & Deferred Deadlines */}
              {data.certs.length > 0 && (
                <div className="overflow-x-auto rounded-lg border bg-card">
                  <div className="border-b px-3.5 py-2.5">
                    <h3 className="text-xs font-semibold text-subtle-foreground">
                      Certificates, expiries &amp; upcoming deadlines
                    </h3>
                  </div>
                  <table className="w-full border-collapse text-left text-meta">
                    <thead>
                      <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
                        <th className="px-3.5 py-2">Status</th>
                        <th className="px-3.5 py-2">Item</th>
                        <th className="px-3.5 py-2">Kind</th>
                        <th className="px-3.5 py-2">Host</th>
                        <th className="px-3.5 py-2">Expires / Due</th>
                        <th className="px-3.5 py-2">Detail</th>
                      </tr>
                    </thead>
                    <tbody className="divide-y">
                      {data.certs.map((c) => (
                        <tr key={`${c.host}:${c.kind}:${c.name}`}>
                          <td className="px-3.5 py-2">
                            <Badge variant={c.ok ? "ok" : "warn"} shape="tag">
                              {c.ok ? "ok" : "expiring"}
                            </Badge>
                          </td>
                          <td className="px-3.5 py-2 font-medium text-foreground">{c.name}</td>
                          <td className="px-3.5 py-2">
                            <Badge variant="outline" shape="tag" className="font-mono">
                              {c.kind}
                            </Badge>
                          </td>
                          <td className="px-3.5 py-2 font-mono text-xs text-muted-foreground">
                            {c.host}
                          </td>
                          <td className="px-3.5 py-2 text-xs whitespace-nowrap text-muted-foreground">
                            {c.expires_at ? (
                              <>
                                <AbsTime at={c.expires_at} /> (<RelTime at={c.expires_at} />)
                              </>
                            ) : (
                              "—"
                            )}
                          </td>
                          <td className="px-3.5 py-2 text-muted-foreground">{c.detail ?? "—"}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </section>
          </div>
        )}
      </QueryFrame>
    </div>
  );
}
