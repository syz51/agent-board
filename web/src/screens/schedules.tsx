import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getRouteApi, useNavigate } from "@tanstack/react-router";
import { useMemo } from "react";
import { api } from "@/api/client.ts";
import {
  SCHEDULE_CATEGORIES,
  SCHEDULE_SOURCES,
  type ScheduleState,
  type ScheduledJobSnapshot,
} from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import { Dialog, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog.tsx";
import { NativeSelectOption } from "@/components/ui/native-select.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { useNow } from "@/live/runtime.ts";
import { safeHref } from "@/text/markdown.tsx";
import {
  AbsTime,
  Empty,
  ErrorNotice,
  formatUntil,
  PageHeader,
  QueryFrame,
  RelTime,
  useTitle,
} from "@/ui/common.tsx";
import { FilterSelect, FilterToggle, SearchField } from "@/ui/filters.tsx";
import { Icon } from "@/ui/icons.tsx";

const schedulesRoute = getRouteApi("/schedules");

function stateMeta(state: ScheduleState): {
  label: string;
  pill: string;
  dot: string;
  rank: number;
} {
  switch (state) {
    case "failing":
      return {
        label: "Failing",
        pill: "bg-status-blocked-soft text-status-blocked",
        dot: "bg-status-blocked",
        rank: 0,
      };
    case "overdue":
      return {
        label: "Overdue",
        pill: "bg-amber-soft text-amber",
        dot: "bg-amber",
        rank: 1,
      };
    case "running":
      return {
        label: "Running",
        pill: "bg-status-doing-soft text-status-doing",
        dot: "bg-status-doing",
        rank: 2,
      };
    case "ok":
      return {
        label: "Healthy",
        pill: "bg-status-ready-soft text-status-ready",
        dot: "bg-status-ready",
        rank: 3,
      };
    case "never_run":
      return {
        label: "Never run",
        pill: "bg-status-open-soft text-status-open",
        dot: "bg-status-open",
        rank: 4,
      };
    case "disabled":
      return {
        label: "Disabled",
        pill: "bg-status-done-soft text-status-done",
        dot: "bg-status-done",
        rank: 5,
      };
    default:
      return {
        label: "Unknown",
        pill: "bg-secondary text-muted-foreground",
        dot: "bg-muted-foreground",
        rank: 6,
      };
  }
}

function ScheduleStatePill({ state }: { state: ScheduleState }) {
  const tone = stateMeta(state);
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

function formatDurationMs(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms)) return "—";
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${(ms / 60_000).toFixed(1)}m`;
}

function safeJobHref(raw: string | null): string | null {
  if (!raw) return null;
  if (raw.startsWith("t3://") || (raw.startsWith("/") && !raw.startsWith("//"))) return raw;
  return safeHref(raw);
}

/**
 * Extract daily fixed clock time(s) in JST minutes-from-midnight `[0..1440)` for the 24h cadence
 * timeline view. Returns `[]` for high-frequency interval jobs so they appear in the interval list.
 */
function jstTimelineMinutes(job: ScheduledJobSnapshot): number[] {
  // High-frequency intervals (< 6h) clutter a 24h fixed-clock grid; keep them in the periodic list.
  if (job.expected_interval_secs !== null && job.expected_interval_secs < 21_600) {
    return [];
  }

  // 1. Prefer the authoritative UTC `next_run_at` converted to Asia/Tokyo (UTC+9) so UTC/CST
  //    schedules on Linux hosts or T3 jobs never plot 9 hours off on the JST grid.
  if (job.next_run_at) {
    const ms = Date.parse(job.next_run_at);
    if (Number.isFinite(ms)) {
      const jst = new Date(ms + 9 * 3_600_000);
      return [jst.getUTCHours() * 60 + jst.getUTCMinutes()];
    }
  }

  // 2. Fallback to explicit HH:MM from schedule_human when `next_run_at` is absent.
  const isUtc = /\bUTC\b/i.test(job.schedule_human);
  const hits = new Set<number>();
  for (const m of job.schedule_human.matchAll(/\b([01]?\d|2[0-3]):([0-5]\d)\b/g)) {
    const h = Number(m[1]);
    const min = Number(m[2]);
    if (Number.isInteger(h) && Number.isInteger(min)) {
      const mins = h * 60 + min;
      hits.add(isUtc ? (mins + 9 * 60) % 1440 : mins);
    }
  }
  return [...hits].sort((a, b) => a - b);
}

function Timeline24hView({
  jobs,
  now,
  onSelect,
}: {
  jobs: ScheduledJobSnapshot[];
  now: number;
  onSelect: (job: ScheduledJobSnapshot) => void;
}) {
  const { lanes, hourlyCounts, periodic } = useMemo(() => {
    const byHost = new Map<string, { job: ScheduledJobSnapshot; minute: number }[]>();
    const counts = new Array<number>(24).fill(0);
    const intervalJobs: ScheduledJobSnapshot[] = [];

    for (const job of jobs) {
      if (!job.enabled) continue;
      const mins = jstTimelineMinutes(job);
      if (mins.length === 0) {
        intervalJobs.push(job);
        continue;
      }
      const list = byHost.get(job.host) ?? [];
      for (const minute of mins) {
        list.push({ job, minute });
        const hour = Math.min(23, Math.floor(minute / 60));
        counts[hour] = (counts[hour] ?? 0) + 1;
      }
      byHost.set(job.host, list);
    }

    const sortedLanes = [...byHost.entries()].sort((a, b) => a[0].localeCompare(b[0]));
    return { lanes: sortedLanes, hourlyCounts: counts, periodic: intervalJobs };
  }, [jobs]);

  const jstNow = new Date(now + 9 * 3_600_000);
  const nowMinutes = jstNow.getUTCHours() * 60 + jstNow.getUTCMinutes();

  const width = 920;
  const labelW = 148;
  const rightPad = 20;
  const topPad = 28;
  const laneH = 44;
  const plotW = width - labelW - rightPad;
  const height = topPad + Math.max(1, lanes.length) * laneH + 18;
  const xForMin = (m: number) => labelW + (m / 1440) * plotW;

  const dotFill = (state: ScheduleState) =>
    state === "failing"
      ? "fill-status-blocked"
      : state === "overdue"
        ? "fill-amber"
        : state === "running"
          ? "fill-status-doing"
          : "fill-status-ready";

  return (
    <div className="grid gap-4">
      <div className="overflow-x-auto rounded-lg border bg-card p-4">
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2 text-xs text-muted-foreground">
          <span className="font-semibold text-subtle-foreground">
            24-hour fixed-time schedule map (JST / UTC+9)
          </span>
          <span>Highlighted bands mark hours with 2+ scheduled runs across hosts.</span>
        </div>

        {lanes.length === 0 ? (
          <p className="py-6 text-center text-meta text-muted-foreground">
            No fixed-time scheduled jobs match the active filters.
          </p>
        ) : (
          <svg
            viewBox={`0 0 ${width} ${height}`}
            className="w-full min-w-[680px]"
            role="img"
            aria-label="24-hour JST scheduled jobs cadence timeline"
          >
            {/* Contention bands (hours with >= 2 runs) */}
            {hourlyCounts.map((count, hour) =>
              count >= 2 ? (
                <rect
                  key={`band-${hour}`}
                  x={xForMin(hour * 60)}
                  y={topPad - 4}
                  width={plotW / 24}
                  height={lanes.length * laneH + 8}
                  className="fill-amber-soft/75"
                />
              ) : null,
            )}

            {/* Hour tick grid */}
            {[0, 3, 6, 9, 12, 15, 18, 21, 24].map((hour) => {
              const x = xForMin(hour * 60);
              return (
                <g key={`tick-${hour}`}>
                  <line
                    x1={x}
                    x2={x}
                    y1={topPad - 4}
                    y2={topPad + lanes.length * laneH}
                    className="stroke-border stroke-[0.8]"
                  />
                  <text
                    x={x}
                    y={16}
                    textAnchor="middle"
                    className="fill-muted-foreground font-mono text-[10px] tabular-nums"
                  >
                    {String(hour).padStart(2, "0")}:00
                  </text>
                </g>
              );
            })}

            {/* Current JST time line */}
            <line
              x1={xForMin(nowMinutes)}
              x2={xForMin(nowMinutes)}
              y1={topPad - 6}
              y2={topPad + lanes.length * laneH + 4}
              className="stroke-signal stroke-[1.5]"
              strokeDasharray="3 2"
            />

            {/* Host swimlanes */}
            {lanes.map(([host, items], idx) => {
              const yMid = topPad + idx * laneH + laneH / 2;
              return (
                <g key={host}>
                  <line
                    x1={labelW}
                    x2={width - rightPad}
                    y1={topPad + (idx + 1) * laneH}
                    y2={topPad + (idx + 1) * laneH}
                    className="stroke-border stroke-[0.7]"
                  />
                  <text
                    x={labelW - 10}
                    y={yMid + 4}
                    textAnchor="end"
                    className="fill-foreground font-mono text-[11px] font-medium"
                  >
                    {host}
                  </text>

                  {items.map(({ job, minute }, jIdx) => {
                    const cx = xForMin(minute);
                    // Offset vertically if two jobs on the same host share a close slot.
                    const dy = items.some(
                      (other, oIdx) => oIdx < jIdx && Math.abs(other.minute - minute) < 25,
                    )
                      ? 10
                      : 0;
                    const hh = String(Math.floor(minute / 60)).padStart(2, "0");
                    const mm = String(minute % 60).padStart(2, "0");
                    return (
                      <g
                        key={`${job.job_key}:${minute}`}
                        className="cursor-pointer"
                        onClick={() => onSelect(job)}
                        role="button"
                        tabIndex={0}
                        onKeyDown={(e) => {
                          if (e.key === "Enter" || e.key === " ") {
                            e.preventDefault();
                            onSelect(job);
                          }
                        }}
                      >
                        <title>{`${job.name} (${job.host}) · ${hh}:${mm} JST · ${job.schedule_human} · ${job.state}`}</title>
                        <circle
                          cx={cx}
                          cy={yMid + dy}
                          r={6}
                          className={cn(dotFill(job.state), "stroke-card stroke-[1.5]")}
                        />
                      </g>
                    );
                  })}
                </g>
              );
            })}
          </svg>
        )}
      </div>

      {/* Periodic / interval jobs strip */}
      {periodic.length > 0 && (
        <div className="rounded-lg border bg-card p-4">
          <h3 className="mb-2.5 text-xs font-semibold text-subtle-foreground">
            Periodic / interval jobs ({periodic.length})
          </h3>
          <div className="flex flex-wrap gap-2">
            {periodic.map((job) => (
              <button
                key={job.job_key}
                type="button"
                onClick={() => onSelect(job)}
                className="inline-flex cursor-pointer items-center gap-2 rounded-md border bg-muted/60 px-2.5 py-1.5 text-left text-xs hover:bg-muted"
              >
                <ScheduleStatePill state={job.state} />
                <span className="font-medium text-foreground">{job.name}</span>
                <span className="font-mono text-muted-foreground">{job.host}</span>
                <span className="text-muted-foreground">· {job.schedule_human}</span>
              </button>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function JobDetailDrawer({
  job,
  onClose,
}: {
  job: ScheduledJobSnapshot | null;
  onClose: () => void;
}) {
  const historyQuery = useQuery({
    ...queries.scheduleHistory(job?.job_key ?? ""),
    enabled: job !== null,
  });

  const href = job ? safeJobHref(job.deep_link) : null;

  return (
    <Dialog
      open={job !== null}
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      className="fixed inset-y-0 right-0 left-auto m-0 h-dvh max-h-dvh w-full max-w-xl rounded-none rounded-l-lg border-y-0 border-r-0 p-0 shadow-overlay sm:max-w-xl"
    >
      {job && (
        <div className="flex h-full flex-col">
          <DialogHeader className="border-b px-5 py-4">
            <div className="flex items-start justify-between gap-3">
              <div className="grid gap-1.5">
                <div className="flex flex-wrap items-center gap-1.5">
                  <ScheduleStatePill state={job.state} />
                  <Badge variant="secondary" shape="tag" className="font-mono">
                    {job.source}
                  </Badge>
                  <Badge variant="outline" shape="tag">
                    {job.category}
                  </Badge>
                  {job.host_stale && (
                    <Badge variant="warn" shape="tag">
                      host stale
                    </Badge>
                  )}
                </div>
                <DialogTitle className="text-title">{job.name}</DialogTitle>
                <DialogDescription className="font-mono text-xs">
                  {job.host} · {job.id}
                </DialogDescription>
              </div>
              <Button variant="ghost" size="sm" onClick={onClose} aria-label="Close job details">
                <Icon name="cross" />
              </Button>
            </div>
          </DialogHeader>

          <div className="grid flex-1 content-start gap-5 overflow-y-auto px-5 py-4">
            {/* Schedule metadata */}
            <dl className="grid grid-cols-2 gap-3 rounded-lg border bg-muted/50 p-3.5 text-xs">
              <div>
                <dt className="text-muted-foreground">Cadence</dt>
                <dd className="mt-0.5 font-medium text-foreground">{job.schedule_human}</dd>
              </div>
              <div>
                <dt className="text-muted-foreground">Raw spec</dt>
                <dd className="mt-0.5 font-mono break-all text-foreground">
                  {job.schedule_raw || "—"}
                </dd>
              </div>
              <div>
                <dt className="text-muted-foreground">Last run</dt>
                <dd className="mt-0.5 text-foreground">
                  {job.last_run_at ? (
                    <>
                      <AbsTime at={job.last_run_at} /> (<RelTime at={job.last_run_at} />)
                    </>
                  ) : (
                    "Never"
                  )}
                </dd>
              </div>
              <div>
                <dt className="text-muted-foreground">Next run</dt>
                <dd className="mt-0.5 text-foreground">
                  {job.next_run_at ? (
                    <>
                      <AbsTime at={job.next_run_at} /> (<RelTime at={job.next_run_at} />)
                    </>
                  ) : (
                    "—"
                  )}
                </dd>
              </div>
              <div>
                <dt className="text-muted-foreground">Last exit / duration</dt>
                <dd className="mt-0.5 font-mono text-foreground tabular-nums">
                  {job.last_exit_code !== null ? `exit ${job.last_exit_code}` : "—"} ·{" "}
                  {formatDurationMs(job.last_duration_ms)}
                </dd>
              </div>
              <div>
                <dt className="text-muted-foreground">Total runs / reported</dt>
                <dd className="mt-0.5 text-foreground tabular-nums">
                  {job.run_count ?? "—"} runs · <RelTime at={job.reported_at} />
                </dd>
              </div>
            </dl>

            {/* Failing stderr/stdout tail */}
            {job.last_error && (
              <div className="grid gap-1.5">
                <h3 className="text-xs font-semibold text-error">Last error output (redacted)</h3>
                <pre className="max-h-56 overflow-auto rounded-md border border-error-border bg-error-soft p-3 font-mono text-xs whitespace-pre-wrap text-error">
                  {job.last_error}
                </pre>
              </div>
            )}

            {/* Command / prompt preview */}
            {job.command_preview && (
              <div className="grid gap-1.5">
                <h3 className="text-xs font-semibold text-subtle-foreground">
                  Command / prompt preview
                </h3>
                <pre className="max-h-40 overflow-auto rounded-md border bg-muted p-3 font-mono text-xs whitespace-pre-wrap text-foreground">
                  {job.command_preview}
                </pre>
              </div>
            )}

            {/* Deep link */}
            {href && (
              <div>
                <a
                  href={href}
                  target={href.startsWith("http") ? "_blank" : undefined}
                  rel={href.startsWith("http") ? "noopener noreferrer" : undefined}
                  className="inline-flex items-center gap-1.5 rounded-md border border-input bg-card px-3 py-1.5 text-xs font-medium text-foreground hover:bg-muted hover:no-underline"
                >
                  <Icon name="external" className="size-3.5" />
                  Open source thread / target
                </a>
              </div>
            )}

            {/* Ring-buffered Run History */}
            <div className="grid gap-2">
              <h3 className="text-xs font-semibold text-subtle-foreground">
                Recent run &amp; state history (last 30 transitions)
              </h3>
              <QueryFrame query={historyQuery} label="Loading run history">
                {(history) =>
                  history.entries.length === 0 ? (
                    <p className="rounded-md border bg-muted/40 px-3 py-3 text-meta text-muted-foreground">
                      No state transitions recorded yet.
                    </p>
                  ) : (
                    <div className="overflow-x-auto rounded-md border">
                      <table className="w-full border-collapse text-left text-xs">
                        <thead>
                          <tr className="border-b bg-muted font-medium text-muted-foreground">
                            <th className="px-2.5 py-1.5">When</th>
                            <th className="px-2.5 py-1.5">State</th>
                            <th className="px-2.5 py-1.5 text-right">Exit</th>
                            <th className="px-2.5 py-1.5 text-right">Duration</th>
                          </tr>
                        </thead>
                        <tbody className="divide-y tabular-nums">
                          {history.entries.map((entry) => (
                            <tr key={entry.id} className="align-top">
                              <td className="max-w-[230px] px-2.5 py-1.5 text-muted-foreground">
                                <span className="whitespace-nowrap">
                                  <RelTime at={entry.at} />
                                </span>
                                {entry.error_summary && (
                                  <p className="mt-0.5 truncate font-mono text-2xs text-error">
                                    {entry.error_summary}
                                  </p>
                                )}
                              </td>
                              <td className="px-2.5 py-1.5 whitespace-nowrap">
                                <ScheduleStatePill state={entry.state} />
                              </td>
                              <td className="px-2.5 py-1.5 text-right font-mono text-muted-foreground">
                                {entry.exit_code ?? "—"}
                              </td>
                              <td className="px-2.5 py-1.5 text-right whitespace-nowrap text-muted-foreground">
                                {formatDurationMs(entry.duration_ms)}
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                  )
                }
              </QueryFrame>
            </div>
          </div>
        </div>
      )}
    </Dialog>
  );
}

/**
 * Cross-Machine Scheduled Jobs Console (`/schedules`):
 * 1-click KPI filter strip, faceted filters, Hide OS timers toggle, Table + 24h JST Timeline,
 * and slide-over job detail drawer with ring-buffered history and redacted error tails.
 */
export function SchedulesScreen() {
  useTitle("Schedules");
  const search = schedulesRoute.useSearch();
  const navigate = useNavigate({ from: "/schedules" });
  const queryClient = useQueryClient();
  const now = useNow(30_000);

  const includeOs = search.include_os === "1";
  const viewMode = search.view === "timeline" ? "timeline" : "table";
  const groupBy = search.group ?? "status";

  const schedulesQuery = useQuery(
    queries.schedules({
      host: search.host,
      source: search.source,
      category: search.category,
      status: search.status,
      include_os: includeOs || undefined,
      q: search.q,
    }),
  );

  const refreshMutation = useMutation({
    mutationFn: fetching(() => api.refreshHealth()),
    onSuccess: () => {
      void invalidateTags(queryClient, [tags.schedules, tags.health]);
    },
  });

  const patchSearch = (patch: Partial<typeof search>) => {
    void navigate({
      search: (prev: typeof search) => ({ ...prev, ...patch }),
      replace: true,
    });
  };

  const data = schedulesQuery.data;
  const selectedJob = useMemo(
    () => (search.job && data ? (data.jobs.find((j) => j.job_key === search.job) ?? null) : null),
    [data, search.job],
  );

  const groupedJobs = useMemo(() => {
    const jobs = data?.jobs ?? [];
    if (groupBy === "none") return [["All jobs", jobs] as const];

    const groups = new Map<string, ScheduledJobSnapshot[]>();
    for (const job of jobs) {
      const key =
        groupBy === "host"
          ? job.host
          : groupBy === "category"
            ? job.category
            : stateMeta(job.state).label;
      const list = groups.get(key) ?? [];
      list.push(job);
      groups.set(key, list);
    }

    const entries = [...groups.entries()];
    if (groupBy === "status") {
      const order = [
        "Failing",
        "Overdue",
        "Running",
        "Healthy",
        "Never run",
        "Disabled",
        "Unknown",
      ];
      entries.sort((a, b) => order.indexOf(a[0]) - order.indexOf(b[0]));
    } else {
      entries.sort((a, b) => a[0].localeCompare(b[0]));
    }
    return entries;
  }, [data?.jobs, groupBy]);

  const kpiCards: {
    key: ScheduleState;
    label: string;
    count: number;
    accent: string;
  }[] = data
    ? [
        {
          key: "failing",
          label: "Failing",
          count: data.summary.failing,
          accent:
            data.summary.failing > 0
              ? "border-status-blocked/45 bg-status-blocked-soft/60 text-status-blocked"
              : "text-foreground",
        },
        {
          key: "overdue",
          label: "Overdue",
          count: data.summary.overdue,
          accent:
            data.summary.overdue > 0
              ? "border-amber/45 bg-amber-soft/60 text-amber"
              : "text-foreground",
        },
        {
          key: "running",
          label: "Running",
          count: data.summary.running,
          accent: "text-status-doing",
        },
        {
          key: "ok",
          label: "Healthy",
          count: data.summary.ok,
          accent: "text-status-ready",
        },
        {
          key: "never_run",
          label: "Never run",
          count: data.summary.never_run,
          accent: "text-muted-foreground",
        },
        {
          key: "disabled",
          label: "Disabled",
          count: data.summary.disabled,
          accent: "text-muted-foreground",
        },
      ]
    : [];

  return (
    <div className="grid gap-5">
      <PageHeader
        title="Schedules"
        sub={
          data
            ? `${data.summary.total} active jobs across ${data.hosts.length} ${data.hosts.length === 1 ? "host" : "hosts"} · updated ${formatUntil(data.generated_at, now)}`
            : "Cross-machine launchd, systemd, T3, and cron scheduled jobs."
        }
        aside={
          <div className="flex flex-wrap items-center gap-2">
            <div
              className="inline-flex h-[34px] items-center rounded-md border bg-muted p-0.5 text-xs"
              role="tablist"
              aria-label="Schedules view"
            >
              <button
                type="button"
                role="tab"
                aria-selected={viewMode === "table"}
                onClick={() => patchSearch({ view: undefined })}
                className={cn(
                  "h-full cursor-pointer rounded-sm px-2.5 font-medium transition-colors",
                  viewMode === "table"
                    ? "bg-card text-foreground shadow-xs"
                    : "text-muted-foreground hover:text-foreground",
                )}
              >
                Table
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={viewMode === "timeline"}
                onClick={() => patchSearch({ view: "timeline" })}
                className={cn(
                  "h-full cursor-pointer rounded-sm px-2.5 font-medium transition-colors",
                  viewMode === "timeline"
                    ? "bg-card text-foreground shadow-xs"
                    : "text-muted-foreground hover:text-foreground",
                )}
              >
                24h Timeline
              </button>
            </div>
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
              {refreshMutation.isPending ? "Refreshing…" : "Refresh"}
            </Button>
          </div>
        }
      />

      {refreshMutation.error && (
        <ErrorNotice error={refreshMutation.error} retry={() => refreshMutation.mutate()} compact />
      )}

      {/* 1-click KPI Filter Strip */}
      {data && (
        <div className="grid grid-cols-2 gap-2.5 sm:grid-cols-3 lg:grid-cols-6">
          {kpiCards.map((kpi) => {
            const active = search.status === kpi.key;
            return (
              <button
                key={kpi.key}
                type="button"
                aria-pressed={active}
                onClick={() => patchSearch({ status: active ? undefined : kpi.key })}
                className={cn(
                  "grid cursor-pointer gap-0.5 rounded-lg border bg-card px-3.5 py-2.5 text-left transition-colors hover:bg-muted/60",
                  kpi.accent,
                  active && "ring-2 ring-signal ring-offset-1 ring-offset-background",
                )}
              >
                <span className="text-xs font-medium text-muted-foreground">{kpi.label}</span>
                <span className="text-title font-semibold tabular-nums">{kpi.count}</span>
              </button>
            );
          })}
        </div>
      )}

      {/* Faceted Filter Bar */}
      <div className="flex flex-wrap items-center gap-2">
        <SearchField
          value={search.q ?? ""}
          onChange={(q) => patchSearch({ q: q || undefined })}
          label="Search scheduled jobs"
          placeholder="Filter by job name, unit, host, or command…"
        />
        <FilterSelect
          label="Host"
          value={search.host ?? ""}
          onChange={(host) => patchSearch({ host: host || undefined })}
        >
          <NativeSelectOption value="">All hosts</NativeSelectOption>
          {(data?.hosts ?? []).map((h) => (
            <NativeSelectOption key={h} value={h}>
              {h}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        <FilterSelect
          label="Source"
          value={search.source ?? ""}
          onChange={(source) => patchSearch({ source: source || undefined })}
        >
          <NativeSelectOption value="">All</NativeSelectOption>
          {SCHEDULE_SOURCES.map((s) => (
            <NativeSelectOption key={s} value={s}>
              {s}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        <FilterSelect
          label="Category"
          value={search.category ?? ""}
          onChange={(category) => patchSearch({ category: category || undefined })}
        >
          <NativeSelectOption value="">All</NativeSelectOption>
          {SCHEDULE_CATEGORIES.map((c) => (
            <NativeSelectOption key={c} value={c}>
              {c}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        {viewMode === "table" && (
          <FilterSelect
            label="Group"
            value={groupBy}
            onChange={(group) => patchSearch({ group: group === "status" ? undefined : group })}
          >
            <NativeSelectOption value="status">Status</NativeSelectOption>
            <NativeSelectOption value="host">Host</NativeSelectOption>
            <NativeSelectOption value="category">Category</NativeSelectOption>
            <NativeSelectOption value="none">Flat list</NativeSelectOption>
          </FilterSelect>
        )}
        <FilterToggle
          checked={!includeOs}
          onChange={(hide) => patchSearch({ include_os: hide ? undefined : "1" })}
        >
          Hide OS timers
          {data && data.summary.os_hidden > 0 && !includeOs && (
            <span className="text-xs text-muted-foreground tabular-nums">
              ({data.summary.os_hidden})
            </span>
          )}
        </FilterToggle>
      </div>

      <QueryFrame query={schedulesQuery} label="Loading scheduled jobs">
        {(list) =>
          list.jobs.length === 0 ? (
            <Empty icon="schedules" title="No scheduled jobs match these filters">
              Try clearing a filter or enabling <code>[schedules]</code> on your host collectors.
            </Empty>
          ) : viewMode === "timeline" ? (
            <Timeline24hView
              jobs={list.jobs}
              now={now}
              onSelect={(job) => patchSearch({ job: job.job_key })}
            />
          ) : (
            <div className="grid gap-5">
              {groupedJobs.map(([groupLabel, jobs]) => (
                <section key={groupLabel} className="grid gap-2">
                  {groupBy !== "none" && (
                    <h2 className="flex items-baseline gap-2 text-xs font-semibold tracking-wide text-muted-foreground uppercase">
                      <span>{groupLabel}</span>
                      <span className="tabular-nums">{jobs.length}</span>
                    </h2>
                  )}
                  <div className="overflow-x-auto rounded-lg border bg-card">
                    <table className="w-full border-collapse text-left text-meta">
                      <thead>
                        <tr className="border-b bg-muted text-xs font-medium text-muted-foreground">
                          <th className="px-3.5 py-2">Status</th>
                          <th className="px-3.5 py-2">Job</th>
                          <th className="px-3.5 py-2">Host</th>
                          <th className="px-3.5 py-2">Cadence</th>
                          <th className="px-3.5 py-2">Last run</th>
                          <th className="px-3.5 py-2">Next run</th>
                          <th className="px-3.5 py-2 text-right">Exit / Runs</th>
                        </tr>
                      </thead>
                      <tbody className="divide-y">
                        {jobs.map((job) => (
                          <tr
                            key={job.job_key}
                            onClick={() => patchSearch({ job: job.job_key })}
                            className="cursor-pointer transition-colors hover:bg-muted/60"
                          >
                            <td className="px-3.5 py-2.5 whitespace-nowrap">
                              <ScheduleStatePill state={job.state} />
                            </td>
                            <td className="px-3.5 py-2.5">
                              <div className="flex flex-wrap items-center gap-1.5">
                                <button
                                  type="button"
                                  onClick={(e) => {
                                    e.stopPropagation();
                                    patchSearch({ job: job.job_key });
                                  }}
                                  className="cursor-pointer text-left font-medium text-foreground hover:text-link hover:underline"
                                >
                                  {job.name}
                                </button>
                                <Badge variant="secondary" shape="tag" className="font-mono">
                                  {job.source}
                                </Badge>
                                <Badge variant="outline" shape="tag">
                                  {job.category}
                                </Badge>
                                {job.host_stale && (
                                  <Badge variant="warn" shape="tag">
                                    stale host
                                  </Badge>
                                )}
                              </div>
                              <p className="mt-0.5 max-w-[340px] truncate font-mono text-2xs text-muted-foreground">
                                {job.id}
                                {job.last_error && (
                                  <span className="ml-2 text-error">— {job.last_error}</span>
                                )}
                              </p>
                            </td>
                            <td className="px-3.5 py-2.5 font-mono text-xs whitespace-nowrap text-muted-foreground">
                              {job.host}
                            </td>
                            <td className="px-3.5 py-2.5 whitespace-nowrap text-subtle-foreground">
                              {job.schedule_human}
                            </td>
                            <td className="px-3.5 py-2.5 text-xs whitespace-nowrap text-muted-foreground">
                              {job.last_run_at ? <RelTime at={job.last_run_at} /> : "never"}
                            </td>
                            <td className="px-3.5 py-2.5 text-xs whitespace-nowrap text-muted-foreground">
                              {job.next_run_at ? formatUntil(job.next_run_at, now) : "—"}
                            </td>
                            <td className="px-3.5 py-2.5 text-right font-mono text-xs whitespace-nowrap text-muted-foreground tabular-nums">
                              {job.last_exit_code !== null ? (
                                <span
                                  className={
                                    job.last_exit_code !== 0
                                      ? "font-semibold text-error"
                                      : undefined
                                  }
                                >
                                  rc={job.last_exit_code}
                                </span>
                              ) : (
                                "—"
                              )}
                              {job.run_count !== null ? ` · ${job.run_count}×` : ""}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                </section>
              ))}
            </div>
          )
        }
      </QueryFrame>

      <JobDetailDrawer job={selectedJob} onClose={() => patchSearch({ job: undefined })} />
    </div>
  );
}
