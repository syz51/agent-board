import { useQuery } from "@tanstack/react-query";
import { getRouteApi, Link, useNavigate } from "@tanstack/react-router";
import type {
  DeviceEntry,
  DeviceRole,
  DeviceStatus,
  SchedulerPolicyResponse,
  SchedulerStats,
  Taint,
  UsageBucket,
} from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { NativeSelectOption } from "@/components/ui/native-select.tsx";
import { cn } from "@/lib/utils.ts";
import { queries } from "@/live/query.ts";
import { useNow } from "@/live/runtime.ts";
import { PlainText } from "@/text/markdown.tsx";
import {
  Empty,
  ErrorNotice,
  formatUntil,
  PageHeader,
  QueryFrame,
  RelTime,
  SectionTitle,
  useTitle,
} from "@/ui/common.tsx";
import { FilterSelect } from "@/ui/filters.tsx";

const fleetRoute = getRouteApi("/fleet");

const WINDOW_DAYS = [1, 7, 14, 30] as const;

function parseDays(raw: string | undefined): number {
  const n = Number(raw);
  return Number.isInteger(n) && n >= 1 && n <= 365 ? n : 7;
}

/**
 * CSP-safe SVG horizontal bar (zero inline `style` attributes). Supports an optional vertical
 * marker line (such as the weekly pace day line or reserve ceiling).
 */
function MeterBar({
  percent,
  tone,
  markerPercent,
  reservePercent,
  label,
}: {
  percent: number;
  tone: "doing" | "ready" | "warn" | "blocked";
  markerPercent?: number | undefined;
  reservePercent?: number | undefined;
  label: string;
}) {
  const clamped = Math.max(0, Math.min(100, Number.isFinite(percent) ? percent : 0));
  const fillClass =
    tone === "blocked"
      ? "fill-status-blocked"
      : tone === "warn"
        ? "fill-amber"
        : tone === "ready"
          ? "fill-status-ready"
          : "fill-status-doing";
  const markerX =
    markerPercent !== undefined && Number.isFinite(markerPercent)
      ? Math.max(1, Math.min(99, markerPercent))
      : undefined;
  const reserveStart =
    reservePercent !== undefined && Number.isFinite(reservePercent) && reservePercent > 0
      ? Math.max(0, Math.min(100, 100 - reservePercent))
      : undefined;

  return (
    <svg
      viewBox="0 0 100 6"
      preserveAspectRatio="none"
      className="h-2 w-full overflow-hidden rounded-full bg-secondary"
      role="img"
      aria-label={label}
    >
      {reserveStart !== undefined && (
        <rect
          x={reserveStart}
          y={0}
          width={100 - reserveStart}
          height={6}
          className="fill-amber-soft"
        />
      )}
      {clamped > 0 && <rect x={0} y={0} width={clamped} height={6} rx={2} className={fillClass} />}
      {markerX !== undefined && (
        <line x1={markerX} x2={markerX} y1={0} y2={6} className="stroke-foreground stroke-[1.25]" />
      )}
    </svg>
  );
}

function deviceStatusTone(status: DeviceStatus): { label: string; pill: string; dot: string } {
  switch (status) {
    case "online":
      return {
        label: "Online",
        pill: "bg-status-ready-soft text-status-ready",
        dot: "bg-status-ready",
      };
    case "stale":
      return {
        label: "Stale",
        pill: "bg-amber-soft text-amber",
        dot: "bg-amber",
      };
    case "offline":
      return {
        label: "Offline",
        pill: "bg-status-done-soft text-status-done",
        dot: "bg-status-done",
      };
    default:
      return {
        label: "Unknown",
        pill: "bg-status-open-soft text-status-open",
        dot: "bg-status-open",
      };
  }
}

function DeviceStatusPill({ status }: { status: DeviceStatus }) {
  const tone = deviceStatusTone(status);
  return (
    <span
      className={cn(
        "inline-flex h-5.5 items-center gap-1.5 rounded-full px-2 text-xs font-[550] whitespace-nowrap",
        tone.pill,
      )}
    >
      <span className={cn("size-1.75 flex-none rounded-full", tone.dot)} aria-hidden="true" />
      {tone.label}
    </span>
  );
}

function roleBadgeVariant(role: DeviceRole): "accent" | "ok" | "review" | "outline" {
  switch (role) {
    case "control":
      return "review";
    case "runner":
      return "ok";
    case "service_host":
      return "accent";
    default:
      return "outline";
  }
}

function formatTaint(t: Taint): string {
  const kv = t.value ? `${t.key}=${t.value}` : t.key;
  return `${kv}:${t.effect}`;
}

function formatSecs(value: number | null): string {
  if (value === null || !Number.isFinite(value)) return "—";
  if (value < 60) return `${value.toFixed(1)}s`;
  return `${(value / 60).toFixed(1)}m`;
}

function formatPct(rate: number | null): string {
  if (rate === null || !Number.isFinite(rate)) return "—";
  return `${(rate * 100).toFixed(1)}%`;
}

/**
 * Fleet Health screen (`/fleet`): fleet nodes (roles, live status, taints, weighted capacity),
 * per-provider quota buckets with weekly pace/reserve, and scheduler telemetry + active policy.
 */
export function FleetScreen() {
  useTitle("Fleet");
  const search = fleetRoute.useSearch();
  const navigate = useNavigate({ from: "/fleet" });
  const days = parseDays(search.days);

  const devicesQuery = useQuery(queries.devices());
  const bucketsQuery = useQuery(queries.usageBuckets());
  const statsQuery = useQuery(queries.schedulerStats(days));
  const policyQuery = useQuery(queries.schedulerPolicy());
  const now = useNow(30_000);

  const headerSub = (() => {
    const devs = devicesQuery.data;
    if (!devs) return "Fleet nodes, quota buckets, and scheduler telemetry.";
    const online = devs.filter((d) => d.status === "online").length;
    const stale = devs.filter((d) => d.status === "stale").length;
    const totalCap = devs.reduce((sum, d) => sum + (d.capacity?.weight_units ?? 0), 0);
    const totalFree = devs.reduce(
      (sum, d) => sum + (d.status === "online" ? (d.free_units ?? 0) : 0),
      0,
    );
    const policy = policyQuery.data?.version ? ` · policy ${policyQuery.data.version}` : "";
    return `${online} online · ${stale} stale · ${totalFree}/${totalCap} free units${policy}`;
  })();

  return (
    <div className="grid gap-7">
      <PageHeader
        title="Fleet"
        sub={headerSub}
        aside={
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
            {WINDOW_DAYS.map((d) => (
              <NativeSelectOption key={d} value={String(d)}>
                Last {d} {d === 1 ? "day" : "days"}
              </NativeSelectOption>
            ))}
          </FilterSelect>
        }
      />

      {/* Section 1: Fleet Nodes */}
      <section aria-labelledby="fleet-nodes-heading">
        <SectionTitle
          id="fleet-nodes-heading"
          {...(devicesQuery.data ? { count: devicesQuery.data.length } : {})}
        >
          Nodes
        </SectionTitle>
        <QueryFrame query={devicesQuery} label="Loading fleet devices">
          {(devices) =>
            devices.length === 0 ? (
              <Empty icon="fleet" title="No fleet devices registered">
                Devices appear here once enrolled and heartbeating via board watch.
              </Empty>
            ) : (
              <div className="grid grid-cols-1 gap-3.5 md:grid-cols-2">
                {devices.map((device) => (
                  <DeviceCard key={device.name} device={device} />
                ))}
              </div>
            )
          }
        </QueryFrame>
      </section>

      {/* Section 2: Quota Buckets */}
      <section aria-labelledby="fleet-quota-heading">
        <SectionTitle
          id="fleet-quota-heading"
          {...(bucketsQuery.data ? { count: bucketsQuery.data.length } : {})}
        >
          Quota buckets
        </SectionTitle>
        <QueryFrame query={bucketsQuery} label="Loading usage buckets">
          {(buckets) =>
            buckets.length === 0 ? (
              <Empty icon="clock" title="No usage buckets reported yet">
                Daemons report per-provider 5-hour and weekly quota readings automatically.
              </Empty>
            ) : (
              <div className="grid grid-cols-1 gap-3.5 md:grid-cols-2">
                {buckets.map((bucket) => (
                  <QuotaBucketCard
                    key={`${bucket.provider}:${bucket.account}`}
                    bucket={bucket}
                    now={now}
                  />
                ))}
              </div>
            )
          }
        </QueryFrame>
      </section>

      {/* Section 3: Scheduler Policy & Telemetry */}
      <section aria-labelledby="fleet-scheduler-heading" className="grid gap-4">
        <SectionTitle id="fleet-scheduler-heading">Scheduler policy &amp; telemetry</SectionTitle>

        {policyQuery.error && (
          <ErrorNotice error={policyQuery.error} retry={() => void policyQuery.refetch()} compact />
        )}
        {policyQuery.data && <PolicyCard policy={policyQuery.data} />}

        <QueryFrame query={statsQuery} label="Loading scheduler telemetry">
          {(stats) => <SchedulerTelemetryView stats={stats} />}
        </QueryFrame>
      </section>
    </div>
  );
}

function DeviceCard({ device }: { device: DeviceEntry }) {
  const totalUnits = device.capacity?.weight_units ?? null;
  const freeUnits = device.free_units;
  const activeUnits =
    totalUnits !== null && freeUnits !== null ? Math.max(0, totalUnits - freeUnits) : null;
  const loadPct =
    totalUnits !== null && totalUnits > 0 && activeUnits !== null
      ? Math.min(100, (activeUnits / totalUnits) * 100)
      : 0;
  const barTone = loadPct >= 90 ? "blocked" : loadPct >= 70 ? "warn" : "doing";

  const hwParts = [
    device.os && device.arch ? `${device.os}/${device.arch}` : (device.os ?? device.arch),
    device.capacity?.cpu_cores ? `${device.capacity.cpu_cores}c` : null,
    device.capacity?.mem_gib ? `${device.capacity.mem_gib} GiB` : null,
  ].filter(Boolean);

  return (
    <article className="grid content-start gap-3 rounded-lg border bg-card p-4">
      <header className="flex flex-wrap items-start justify-between gap-2">
        <div className="grid gap-1">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="ident text-sm font-semibold text-foreground">{device.name}</h3>
            <DeviceStatusPill status={device.status} />
            <Badge variant={roleBadgeVariant(device.role)} shape="tag">
              {device.role}
            </Badge>
            {device.drain && (
              <Badge variant="warn" shape="tag">
                draining
              </Badge>
            )}
          </div>
          <p className="text-meta text-muted-foreground">
            {hwParts.length > 0 ? hwParts.join(" · ") : "Hardware unreported"}
            {device.preference !== 0 ? ` · pref ${device.preference}` : ""}
          </p>
        </div>
        <span className="text-xs whitespace-nowrap text-muted-foreground">
          {device.heartbeat_at ? (
            <>
              heartbeat <RelTime at={device.heartbeat_at} />
            </>
          ) : (
            "no heartbeat"
          )}
        </span>
      </header>

      {totalUnits !== null && (
        <div className="grid gap-1">
          <div className="flex items-center justify-between text-xs text-muted-foreground tabular-nums">
            <span>Weighted load</span>
            <span className="font-medium text-foreground">
              {activeUnits !== null
                ? `${activeUnits} / ${totalUnits} units`
                : `${totalUnits} units cap`}
              {freeUnits !== null ? ` (${freeUnits} free)` : ""}
            </span>
          </div>
          <MeterBar
            percent={loadPct}
            tone={barTone}
            label={`${device.name} weighted load ${Math.round(loadPct)}%`}
          />
        </div>
      )}

      {device.taints.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="text-xs font-medium text-muted-foreground">Taints:</span>
          {device.taints.map((t) => (
            <Badge
              key={formatTaint(t)}
              variant={t.effect === "no_schedule" ? "bad" : "warn"}
              shape="tag"
              className="font-mono"
            >
              {formatTaint(t)}
            </Badge>
          ))}
        </div>
      )}

      <div className="flex flex-wrap items-center justify-between gap-2 border-t pt-2.5 text-xs text-muted-foreground">
        <div className="flex flex-wrap items-center gap-1.5">
          <span>Runners:</span>
          {device.runners.length > 0 ? (
            device.runners.map((runner) => (
              <Badge key={runner} variant="outline" shape="tag" className="font-mono">
                {runner}
              </Badge>
            ))
          ) : (
            <span>none</span>
          )}
        </div>
        <span className="tabular-nums">
          {device.repos.length} {device.repos.length === 1 ? "repo" : "repos"}
        </span>
      </div>

      {device.repos.length > 0 && (
        <details className="text-xs text-muted-foreground">
          <summary className="cursor-pointer hover:text-foreground">
            Mapped repositories &amp; capabilities
          </summary>
          <div className="mt-1.5 grid gap-1.5 rounded-sm bg-muted p-2.5">
            <div>
              <p className="font-medium text-subtle-foreground">Repos</p>
              <ul className="mt-0.5 grid gap-0.5 font-mono">
                {device.repos.map((repo) => (
                  <li key={repo} className="truncate">
                    {repo}
                  </li>
                ))}
              </ul>
            </div>
            {device.capabilities.length > 0 && (
              <div>
                <p className="font-medium text-subtle-foreground">Capabilities</p>
                <p className="mt-0.5 font-mono">{device.capabilities.join(", ")}</p>
              </div>
            )}
          </div>
        </details>
      )}
    </article>
  );
}

function QuotaBucketCard({ bucket, now }: { bucket: UsageBucket; now: number }) {
  const five = bucket.five_hour;
  const week = bucket.seven_day;
  const pace = bucket.pace;
  const reserve = pace?.reserve ?? bucket.reserve;

  return (
    <article className="grid content-start gap-3 rounded-lg border bg-card p-4">
      <header className="flex flex-wrap items-baseline justify-between gap-2">
        <h3 className="ident text-sm font-semibold text-foreground">
          {bucket.provider}/{bucket.account}
        </h3>
        <span className="text-xs text-muted-foreground">
          {bucket.read_at ? (
            <>
              read <RelTime at={bucket.read_at} />
              {bucket.device ? ` by ${bucket.device}` : ""}
            </>
          ) : (
            "reserve only"
          )}
        </span>
      </header>

      {five && (
        <div className="grid gap-1">
          <div className="flex items-center justify-between gap-2 text-xs tabular-nums">
            <span className="font-medium text-subtle-foreground">5-hour window</span>
            <span className="text-foreground">
              <strong className="font-semibold">{five.percent.toFixed(1)}%</strong>
              {five.resets_at && (
                <span className="ml-1.5 text-muted-foreground">
                  · resets {formatUntil(five.resets_at, now)}
                </span>
              )}
            </span>
          </div>
          <MeterBar
            percent={five.percent}
            tone={five.percent >= 90 ? "blocked" : five.percent >= 75 ? "warn" : "doing"}
            label={`5-hour usage ${five.percent.toFixed(1)}%`}
          />
        </div>
      )}

      {week && (
        <div className="grid gap-1">
          <div className="flex items-center justify-between gap-2 text-xs tabular-nums">
            <span className="font-medium text-subtle-foreground">7-day window</span>
            <span className="text-foreground">
              <strong className="font-semibold">{week.percent.toFixed(1)}%</strong>
              {week.resets_at && (
                <span className="ml-1.5 text-muted-foreground">
                  · resets {formatUntil(week.resets_at, now)}
                </span>
              )}
            </span>
          </div>
          <MeterBar
            percent={week.percent}
            tone={
              pace?.hard_since
                ? "blocked"
                : pace && pace.projected >= pace.line
                  ? "warn"
                  : week.percent >= 90
                    ? "blocked"
                    : "ready"
            }
            markerPercent={pace?.line}
            reservePercent={reserve ?? undefined}
            label={`7-day usage ${week.percent.toFixed(1)}%`}
          />
        </div>
      )}

      {(pace || reserve !== null) && (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t pt-2.5 text-xs text-muted-foreground tabular-nums">
          {reserve !== null && <span>Reserve {reserve.toFixed(1)}%</span>}
          {pace && (
            <>
              <span>Today line {pace.line.toFixed(1)}%</span>
              <span>Projected {pace.projected.toFixed(1)}%</span>
              {pace.hard_since && (
                <Badge variant="bad" shape="tag">
                  Hard line reached
                </Badge>
              )}
            </>
          )}
        </div>
      )}
    </article>
  );
}

function PolicyCard({ policy }: { policy: SchedulerPolicyResponse }) {
  const weights: [string, number][] = [
    ["kind", policy.weights.w_kind],
    ["prio", policy.weights.w_prio],
    ["rank", policy.weights.w_rank],
    ["age", policy.weights.w_age],
    ["fair", policy.weights.w_fair],
    ["loc", policy.weights.w_loc],
    ["pref", policy.weights.w_pref],
    ["backfill", policy.weights.w_backfill],
  ];

  const createdMs = Date.parse(policy.created_at);
  const isBuiltinEpoch = Number.isNaN(createdMs) || createdMs <= 86_400_000;

  return (
    <div className="grid gap-3 rounded-lg border bg-card p-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-medium text-muted-foreground">Active policy</span>
          <Badge variant="accent" shape="tag" className="font-mono">
            {policy.version}
          </Badge>
          <span className="text-xs text-muted-foreground tabular-nums">
            lease {policy.lease_ttl_secs}s · sticky grace {policy.sticky_grace_secs}s
          </span>
        </div>
        <span className="text-xs text-muted-foreground">
          {isBuiltinEpoch ? (
            <>built-in default{policy.created_by ? ` · ${policy.created_by}` : ""}</>
          ) : (
            <>
              updated <RelTime at={policy.created_at} />
              {policy.created_by ? ` by ${policy.created_by}` : ""}
            </>
          )}
        </span>
      </div>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4 lg:grid-cols-8">
        {weights.map(([name, value]) => (
          <div key={name} className="rounded-md border bg-muted px-2.5 py-1.5 text-center">
            <p className="font-mono text-2xs whitespace-nowrap text-muted-foreground">w_{name}</p>
            <p className="text-sm font-semibold text-foreground tabular-nums">{value}</p>
          </div>
        ))}
      </div>
      {policy.notes && (
        <p className="text-meta text-muted-foreground">
          <PlainText className="whitespace-normal" text={policy.notes} />
        </p>
      )}
    </div>
  );
}

function MetricTile({
  label,
  value,
  sub,
}: {
  label: string;
  value: string;
  sub?: string | undefined;
}) {
  return (
    <div className="rounded-lg border bg-card px-3.5 py-3">
      <p className="text-xs text-muted-foreground">{label}</p>
      <p className="mt-0.5 text-title font-semibold text-foreground tabular-nums">{value}</p>
      {sub && <p className="mt-0.5 text-xs text-muted-foreground tabular-nums">{sub}</p>}
    </div>
  );
}

function SchedulerTelemetryView({ stats }: { stats: SchedulerStats }) {
  const stateEntries = Object.entries(stats.runs_by_terminal_state).sort((a, b) => b[1] - a[1]);
  const kindEntries = Object.entries(stats.runs_by_kind).sort((a, b) => b[1] - a[1]);
  const holdEntries = Object.entries(stats.holds_by_reason).sort((a, b) => b[1] - a[1]);

  return (
    <div className="grid gap-4">
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <MetricTile label={`Runs (${stats.days}d)`} value={String(stats.runs_total)} />
        <MetricTile
          label="Orphan rate"
          value={formatPct(stats.orphan_rate)}
          sub={`${stats.total_weight_unit_minutes.toFixed(1)} unit-min`}
        />
        <MetricTile
          label="First-pass merge"
          value={formatPct(stats.first_pass_merge_rate)}
          sub={
            stats.avg_ci_runs_before_merge !== null
              ? `avg ${stats.avg_ci_runs_before_merge.toFixed(2)} CI runs`
              : undefined
          }
        />
        <MetricTile
          label="Warm cache hit"
          value={formatPct(stats.warm_cache_hit_rate)}
          sub={
            stats.avg_review_rounds !== null
              ? `avg ${stats.avg_review_rounds.toFixed(2)} review rounds`
              : undefined
          }
        />
      </div>

      {/* Breakdown strips */}
      <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
        <div className="rounded-lg border bg-card p-3.5">
          <h3 className="mb-2 text-xs font-semibold text-subtle-foreground">Runs by state</h3>
          {stateEntries.length === 0 ? (
            <p className="text-meta text-muted-foreground">No terminal runs in window.</p>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {stateEntries.map(([state, count]) => (
                <Badge key={state} variant="secondary" shape="tag" className="tabular-nums">
                  {state}: {count}
                </Badge>
              ))}
            </div>
          )}
        </div>
        <div className="rounded-lg border bg-card p-3.5">
          <h3 className="mb-2 text-xs font-semibold text-subtle-foreground">Runs by kind</h3>
          {kindEntries.length === 0 ? (
            <p className="text-meta text-muted-foreground">No runs in window.</p>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {kindEntries.map(([kind, count]) => (
                <Badge key={kind} variant="outline" shape="tag" className="tabular-nums">
                  {kind}: {count}
                </Badge>
              ))}
            </div>
          )}
        </div>
        <div className="rounded-lg border bg-card p-3.5">
          <h3 className="mb-2 text-xs font-semibold text-subtle-foreground">Holds by reason</h3>
          {holdEntries.length === 0 ? (
            <p className="text-meta text-muted-foreground">No scheduling holds recorded.</p>
          ) : (
            <div className="flex flex-wrap gap-1.5">
              {holdEntries.map(([reason, count]) => (
                <Badge key={reason} variant="warn" shape="tag" className="tabular-nums">
                  {reason}: {count}
                </Badge>
              ))}
            </div>
          )}
        </div>
      </div>

      {/* Latencies */}
      <div className="overflow-x-auto rounded-lg border bg-card">
        <table className="w-full border-collapse text-left text-sm">
          <thead>
            <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
              <th className="px-3.5 py-2">Latency metric</th>
              <th className="px-3.5 py-2 text-right">p50</th>
              <th className="px-3.5 py-2 text-right">p90</th>
              <th className="px-3.5 py-2 text-right">Samples</th>
            </tr>
          </thead>
          <tbody className="divide-y tabular-nums">
            {[
              ["Ready → Leased", stats.ready_to_leased_secs],
              ["Leased → Running", stats.leased_to_running_secs],
              ["Running duration", stats.running_secs],
              ["Conflict lock wait", stats.conflict_wait_secs],
            ].map(([label, metric]) => {
              const m = metric as SchedulerStats["ready_to_leased_secs"];
              return (
                <tr key={label as string}>
                  <td className="px-3.5 py-2 font-medium text-foreground">{label as string}</td>
                  <td className="px-3.5 py-2 text-right text-subtle-foreground">
                    {formatSecs(m.p50)}
                  </td>
                  <td className="px-3.5 py-2 text-right text-subtle-foreground">
                    {formatSecs(m.p90)}
                  </td>
                  <td className="px-3.5 py-2 text-right text-muted-foreground">{m.count}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {/* Per-device & Per-board tables */}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        <div className="overflow-x-auto rounded-lg border bg-card">
          <div className="border-b px-3.5 py-2.5">
            <h3 className="text-xs font-semibold text-subtle-foreground">Per-device breakdown</h3>
          </div>
          {stats.per_device.length === 0 ? (
            <p className="px-3.5 py-4 text-meta text-muted-foreground">No per-device runs.</p>
          ) : (
            <table className="w-full border-collapse text-left text-meta">
              <thead>
                <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
                  <th className="px-3.5 py-2">Device</th>
                  <th className="px-3.5 py-2 text-right">Runs</th>
                  <th className="px-3.5 py-2 text-right">OK / Fail / Orph</th>
                  <th className="px-3.5 py-2 text-right">Unit-min</th>
                </tr>
              </thead>
              <tbody className="divide-y tabular-nums">
                {stats.per_device.map((d) => (
                  <tr key={d.device}>
                    <td className="px-3.5 py-2">
                      <span className="ident font-medium text-foreground">{d.device}</span>
                    </td>
                    <td className="px-3.5 py-2 text-right">{d.runs}</td>
                    <td className="px-3.5 py-2 text-right text-muted-foreground">
                      {d.succeeded} / {d.failed} / {d.orphaned}
                    </td>
                    <td className="px-3.5 py-2 text-right">{d.weight_unit_minutes.toFixed(1)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>

        <div className="overflow-x-auto rounded-lg border bg-card">
          <div className="border-b px-3.5 py-2.5">
            <h3 className="text-xs font-semibold text-subtle-foreground">Per-board breakdown</h3>
          </div>
          {stats.per_board.length === 0 ? (
            <p className="px-3.5 py-4 text-meta text-muted-foreground">No per-board runs.</p>
          ) : (
            <table className="w-full border-collapse text-left text-meta">
              <thead>
                <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
                  <th className="px-3.5 py-2">Board</th>
                  <th className="px-3.5 py-2 text-right">Runs</th>
                  <th className="px-3.5 py-2 text-right">Wait p50/p90</th>
                  <th className="px-3.5 py-2 text-right">Fair-share</th>
                </tr>
              </thead>
              <tbody className="divide-y tabular-nums">
                {stats.per_board.map((b) => (
                  <tr key={b.board}>
                    <td className="px-3.5 py-2">
                      <Link to="/boards/$slug" params={{ slug: b.board }} className="ident">
                        {b.board}
                      </Link>
                    </td>
                    <td className="px-3.5 py-2 text-right">
                      {b.runs}{" "}
                      <span className="text-xs text-muted-foreground">({b.succeeded} ok)</span>
                    </td>
                    <td className="px-3.5 py-2 text-right text-muted-foreground">
                      {formatSecs(b.ready_to_leased_p50_secs)} /{" "}
                      {formatSecs(b.ready_to_leased_p90_secs)}
                    </td>
                    <td className="px-3.5 py-2 text-right font-medium text-foreground">
                      {b.fair_share_percent.toFixed(1)}%
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
    </div>
  );
}
