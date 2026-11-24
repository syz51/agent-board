import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getRouteApi, Link, useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState } from "react";
import { type ApiFailure, api } from "@/api/client.ts";
import { DecodeError } from "@/api/decode.ts";
import {
  type Agent,
  RUN_KINDS,
  RUN_ORIGINS,
  RUN_STATES,
  type Run,
  type RunEvent,
  type RunEventKind,
  type RunOrigin,
  type RunState,
  runEvent as decodeRunEvent,
  type SteerRunResponse,
  type CancelRunResponse,
} from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import {
  Dialog,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog.tsx";
import { Input } from "@/components/ui/input.tsx";
import { NativeSelectOption } from "@/components/ui/native-select.tsx";
import { Textarea } from "@/components/ui/textarea.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { useNow } from "@/live/runtime.ts";
import { BACKOFF_MS, openEventSource, type OpenSource } from "@/live/stream.ts";
import { PlainText, safeHref } from "@/text/markdown.tsx";
import {
  AbsTime,
  agentIndex,
  Crumbs,
  Empty,
  ErrorNotice,
  PageHeader,
  QueryFrame,
  RelTime,
  SectionTitle,
  useAgents,
  useTitle,
  Who,
} from "@/ui/common.tsx";
import { FilterSelect, FilterToggle } from "@/ui/filters.tsx";
import { Icon } from "@/ui/icons.tsx";

const runsRoute = getRouteApi("/runs");
const runDetailRoute = getRouteApi("/runs/$id");

const ACTIVE_STATES = new Set<RunState>([
  "queued",
  "leased",
  "starting",
  "running",
  "waiting_external",
]);

const LIVE_STEER_STATES = new Set<RunState>(["leased", "starting", "running", "waiting_external"]);

const RUN_EVENTS_CAP = 200;

export function isActiveRunState(state: RunState): boolean {
  return ACTIVE_STATES.has(state);
}

export function isSteerableRunState(state: RunState): boolean {
  return LIVE_STEER_STATES.has(state);
}

export function useActiveRuns() {
  return useQuery(queries.activeRuns());
}

/** Formats a duration in seconds as `45s`, `12m 5s`, `2h 10m`, or `3d 4h`. */
export function formatDurationMs(ms: number): string {
  const secs = Math.max(0, Math.floor(ms / 1000));
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) {
    const m = Math.floor(secs / 60);
    const s = secs % 60;
    return s === 0 ? `${m}m` : `${m}m ${s}s`;
  }
  if (secs < 86_400) {
    const h = Math.floor(secs / 3600);
    const m = Math.floor((secs % 3600) / 60);
    return m === 0 ? `${h}h` : `${h}h ${m}m`;
  }
  const d = Math.floor(secs / 86_400);
  const h = Math.floor((secs % 86_400) / 3600);
  return h === 0 ? `${d}d` : `${d}d ${h}h`;
}

function runStateTone(state: RunState): {
  label: string;
  dot: string;
  pill: string;
} {
  switch (state) {
    case "running":
      return {
        label: "Running",
        dot: "bg-status-doing border-status-doing",
        pill: "bg-status-doing-soft text-status-doing",
      };
    case "starting":
      return {
        label: "Starting",
        dot: "bg-status-doing border-status-doing",
        pill: "bg-status-doing-soft text-status-doing",
      };
    case "leased":
      return {
        label: "Leased",
        dot: "border-status-doing bg-transparent",
        pill: "bg-status-doing-soft text-status-doing",
      };
    case "queued":
      return {
        label: "Queued",
        dot: "border-status-open bg-transparent border-dashed",
        pill: "bg-status-open-soft text-status-open",
      };
    case "waiting_external":
      return {
        label: "Waiting",
        dot: "bg-amber border-amber",
        pill: "bg-amber-soft text-amber",
      };
    case "succeeded":
      return {
        label: "Succeeded",
        dot: "bg-status-ready border-status-ready",
        pill: "bg-status-ready-soft text-status-ready",
      };
    case "failed":
      return {
        label: "Failed",
        dot: "bg-status-blocked border-status-blocked",
        pill: "bg-status-blocked-soft text-status-blocked",
      };
    case "orphaned":
      return {
        label: "Orphaned",
        dot: "bg-status-blocked border-status-blocked",
        pill: "bg-status-blocked-soft text-status-blocked",
      };
    case "cancelled":
      return {
        label: "Cancelled",
        dot: "border-status-done bg-transparent border-dotted",
        pill: "bg-status-done-soft text-status-done",
      };
    case "preempted":
      return {
        label: "Preempted",
        dot: "border-amber bg-transparent",
        pill: "bg-amber-soft text-amber",
      };
    default:
      return {
        label: "Unknown",
        dot: "border-status-open bg-transparent",
        pill: "bg-status-open-soft text-status-open",
      };
  }
}

export function RunStatePill({ state }: { state: RunState }) {
  const tone = runStateTone(state);
  return (
    <span
      className={cn(
        "inline-flex h-5.5 items-center gap-1.5 rounded-full px-2 text-xs font-[550] whitespace-nowrap",
        tone.pill,
      )}
    >
      <span
        className={cn("size-1.75 flex-none rounded-full border-[1.5px]", tone.dot)}
        aria-hidden="true"
      />
      {tone.label}
    </span>
  );
}

export function OriginBadge({ origin }: { origin: RunOrigin }) {
  return (
    <Badge variant={origin === "managed" ? "accent" : "outline"} shape="tag">
      {origin}
    </Badge>
  );
}

function eventKindVariant(
  kind: RunEventKind,
): "default" | "secondary" | "outline" | "accent" | "warn" | "ok" | "bad" | "review" | "doing" {
  switch (kind) {
    case "phase":
      return "accent";
    case "tool":
      return "doing";
    case "pr":
      return "review";
    case "done":
      return "ok";
    case "error":
      return "bad";
    case "cancelled":
      return "warn";
    case "note":
    case "message":
      return "secondary";
    default:
      return "outline";
  }
}

function RunElapsed({ run, now }: { run: Run; now: number }) {
  if (run.started_at && run.ended_at) {
    const dur = formatDurationMs(Date.parse(run.ended_at) - Date.parse(run.started_at));
    return (
      <span className="tabular-nums" title={`Ended ${run.ended_at}`}>
        {dur} · <RelTime at={run.ended_at} />
      </span>
    );
  }
  if (run.started_at && isActiveRunState(run.state)) {
    const dur = formatDurationMs(now - Date.parse(run.started_at));
    return (
      <span className="tabular-nums" title={`Started ${run.started_at}`}>
        {dur}
      </span>
    );
  }
  return <RelTime at={run.updated_at} />;
}

/**
 * Unified Runs screen (`/runs`): lists managed and interactive runs across every fleet device,
 * filterable by active-only, origin, kind, device, board, and state.
 */
export function RunsScreen() {
  useTitle("Runs");
  const search = runsRoute.useSearch();
  const navigate = useNavigate({ from: "/runs" });
  const now = useNow(15_000);

  const activeOnly = search.active === "1";
  const queryParams = useMemo(
    () => ({
      active: activeOnly ? true : undefined,
      origin: search.origin,
      kind: search.kind,
      device: search.device,
      board: search.board,
      state: search.state,
      limit: 200,
    }),
    [activeOnly, search.origin, search.kind, search.device, search.board, search.state],
  );

  const runsQuery = useQuery(queries.runs(queryParams));
  const devicesQuery = useQuery(queries.devices());
  const boardsQuery = useQuery(queries.boards());
  const agents = agentIndex(useAgents().data);

  const setSearch = (patch: Partial<typeof search>) => {
    void navigate({
      search: { ...search, ...patch },
      replace: true,
    });
  };

  const hasFilters = Boolean(
    activeOnly || search.origin || search.kind || search.device || search.board || search.state,
  );

  const deviceOptions = useMemo(() => {
    const set = new Set<string>();
    for (const d of devicesQuery.data ?? []) set.add(d.name);
    for (const r of runsQuery.data ?? []) set.add(r.device);
    if (search.device) set.add(search.device);
    return [...set].sort();
  }, [devicesQuery.data, runsQuery.data, search.device]);

  const boardOptions = useMemo(() => {
    const set = new Set<string>();
    for (const b of boardsQuery.data ?? []) set.add(b.slug);
    for (const r of runsQuery.data ?? []) if (r.board) set.add(r.board);
    if (search.board) set.add(search.board);
    return [...set].sort();
  }, [boardsQuery.data, runsQuery.data, search.board]);

  const summary = useMemo(() => {
    const list = runsQuery.data;
    if (!list) return undefined;
    const active = list.filter((r) => isActiveRunState(r.state)).length;
    const managed = list.filter((r) => r.origin === "managed").length;
    const interactive = list.filter((r) => r.origin === "interactive").length;
    return `${list.length} shown · ${active} active · ${managed} managed · ${interactive} interactive`;
  }, [runsQuery.data]);

  return (
    <>
      <PageHeader
        title="Runs"
        sub={summary ?? "Unified managed and interactive runs across every device."}
      />

      <div
        className="-mt-1 mb-5 flex flex-wrap items-center gap-2"
        role="region"
        aria-label="Filter runs"
      >
        <FilterToggle
          checked={activeOnly}
          onChange={(checked) => setSearch({ active: checked ? "1" : undefined })}
        >
          Active only
        </FilterToggle>
        <FilterSelect
          label="Origin"
          value={search.origin ?? ""}
          onChange={(value) => setSearch({ origin: value || undefined })}
        >
          <NativeSelectOption value="">All</NativeSelectOption>
          {RUN_ORIGINS.map((origin) => (
            <NativeSelectOption key={origin} value={origin}>
              {origin}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        <FilterSelect
          label="Kind"
          value={search.kind ?? ""}
          onChange={(value) => setSearch({ kind: value || undefined })}
        >
          <NativeSelectOption value="">All</NativeSelectOption>
          {RUN_KINDS.map((kind) => (
            <NativeSelectOption key={kind} value={kind}>
              {kind}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        <FilterSelect
          label="Device"
          value={search.device ?? ""}
          onChange={(value) => setSearch({ device: value || undefined })}
        >
          <NativeSelectOption value="">All</NativeSelectOption>
          {deviceOptions.map((name) => (
            <NativeSelectOption key={name} value={name}>
              {name}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        <FilterSelect
          label="Board"
          value={search.board ?? ""}
          onChange={(value) => setSearch({ board: value || undefined })}
        >
          <NativeSelectOption value="">All</NativeSelectOption>
          {boardOptions.map((slug) => (
            <NativeSelectOption key={slug} value={slug}>
              {slug}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        <FilterSelect
          label="State"
          value={search.state ?? ""}
          onChange={(value) => setSearch({ state: value || undefined })}
        >
          <NativeSelectOption value="">All</NativeSelectOption>
          {RUN_STATES.map((state) => (
            <NativeSelectOption key={state} value={state}>
              {state}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        {hasFilters && (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() =>
              void navigate({
                search: {},
                replace: true,
              })
            }
          >
            Clear filters
          </Button>
        )}
      </div>

      <QueryFrame query={runsQuery} label="Loading runs">
        {(list) => {
          if (list.length === 0) {
            return (
              <Empty
                icon="runs"
                title={hasFilters ? "No runs match these filters" : "No runs recorded yet"}
              >
                {hasFilters
                  ? "Widen or clear the filters above to see other managed and interactive sessions."
                  : "Runs appear here when daemons dispatch work or when interactive sessions start."}
              </Empty>
            );
          }
          return (
            <div className="edge @container divide-y border-y">
              <div
                className="hidden h-8 grid-cols-[minmax(0,1.8fr)_120px_130px_130px_140px_110px] items-center gap-x-4 bg-muted px-(--gutter) text-xs font-medium text-muted-foreground @min-[920px]:grid"
                aria-hidden="true"
              >
                <span>Run</span>
                <span>State</span>
                <span>Origin / Runner</span>
                <span>Device</span>
                <span>Task / Branch</span>
                <span className="text-right">Cost / Time</span>
              </div>
              {list.map((run) => (
                <RunRow key={run.id} run={run} agents={agents} now={now} />
              ))}
            </div>
          );
        }}
      </QueryFrame>
    </>
  );
}

function RunRow({ run, agents, now }: { run: Run; agents: Map<string, Agent>; now: number }) {
  return (
    <article className="relative grid gap-y-2 px-(--gutter) py-3 hover:bg-muted/60 @min-[920px]:grid-cols-[minmax(0,1.8fr)_120px_130px_130px_140px_110px] @min-[920px]:items-center @min-[920px]:gap-x-4">
      <div className="min-w-0">
        <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
          <Link
            to="/runs/$id"
            params={{ id: run.id }}
            className="font-medium text-foreground before:absolute before:inset-0 hover:no-underline"
          >
            {run.title ? (
              <PlainText className="whitespace-normal" text={run.title} />
            ) : (
              <span className="ident">{run.id}</span>
            )}
          </Link>
          <span className="ident text-xs text-muted-foreground">{run.id}</span>
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-meta text-muted-foreground">
          <Badge variant="secondary" shape="tag">
            {run.kind}
          </Badge>
          {run.agent_id && <Who id={run.agent_id} agents={agents} />}
          {run.board && (
            <Link
              to="/boards/$slug"
              params={{ slug: run.board }}
              className="ident relative z-1 text-xs text-muted-foreground hover:text-foreground"
            >
              {run.board}
            </Link>
          )}
        </div>
      </div>

      <div className="flex items-center gap-2">
        <RunStatePill state={run.state} />
      </div>

      <div className="flex flex-wrap items-center gap-1.5">
        <OriginBadge origin={run.origin} />
        <Badge variant="outline" shape="tag" className="font-mono">
          {run.runner}
        </Badge>
      </div>

      <div className="min-w-0 truncate">
        <span className="ident text-meta text-subtle-foreground" title={run.device}>
          {run.device}
        </span>
      </div>

      <div className="grid min-w-0 gap-0.5">
        {run.task_id ? (
          <Link
            to="/tasks/$id"
            params={{ id: run.task_id }}
            className="ident relative z-1 w-fit text-meta font-medium"
          >
            {run.task_id}
          </Link>
        ) : (
          <span className="text-xs text-muted-foreground">No task</span>
        )}
        {run.worktree_branch && (
          <span
            className="ident truncate text-xs text-muted-foreground"
            title={run.worktree_branch}
          >
            {run.worktree_branch}
          </span>
        )}
      </div>

      <div className="flex items-center justify-between gap-2 text-meta text-muted-foreground tabular-nums @min-[920px]:flex-col @min-[920px]:items-end @min-[920px]:gap-0.5">
        <span className="ident text-xs text-subtle-foreground">{run.cost_units}u</span>
        <RunElapsed run={run} now={now} />
      </div>
    </article>
  );
}

/**
 * Merges incoming structured run events into a deduplicated ascending list capped at 200.
 * Exported for unit tests.
 */
export function mergeRunEvents(
  existing: readonly RunEvent[],
  incoming: readonly RunEvent[],
): RunEvent[] {
  if (incoming.length === 0) return existing.slice(-RUN_EVENTS_CAP);
  const bySeq = new Map<number, RunEvent>();
  for (const ev of existing) bySeq.set(ev.seq, ev);
  for (const ev of incoming) bySeq.set(ev.seq, ev);
  const sorted = [...bySeq.values()].sort((a, b) => a.seq - b.seq);
  return sorted.length > RUN_EVENTS_CAP ? sorted.slice(-RUN_EVENTS_CAP) : sorted;
}

type TailState = "idle" | "connecting" | "live" | "reconnecting" | "ended";

/**
 * Subscribes to `/v1/runs/{id}/events` SSE stream while the run is active, merging incoming
 * structured events into the latest 200 events and refreshing the run record on terminal events.
 */
export function useRunEventTail(
  runId: string,
  active: boolean,
  initial: readonly RunEvent[] | undefined,
  onTerminalEvent?: () => void,
  openSource: OpenSource = openEventSource,
): { events: RunEvent[]; tailState: TailState } {
  const [liveBucket, setLiveBucket] = useState<{ runId: string; events: RunEvent[] }>({
    runId,
    events: [],
  });
  const [streamState, setStreamState] = useState<{ runId: string; state: TailState }>({
    runId,
    state: "connecting",
  });
  const onTerminalRef = useRef(onTerminalEvent);
  const lastSeqRef = useRef(0);

  const merged = useMemo(
    () => mergeRunEvents(initial ?? [], liveBucket.runId === runId ? liveBucket.events : []),
    [initial, liveBucket, runId],
  );
  const newestSeq = merged.at(-1)?.seq ?? 0;

  useEffect(() => {
    onTerminalRef.current = onTerminalEvent;
  }, [onTerminalEvent]);

  useEffect(() => {
    lastSeqRef.current = newestSeq;
  }, [runId, newestSeq]);

  useEffect(() => {
    if (!active) return;
    let stopped = false;
    let attempt = 0;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    let source: { close(): void } | null = null;

    const connect = () => {
      if (stopped) return;
      const url = api.runEventsStreamUrl(
        runId,
        lastSeqRef.current > 0 ? lastSeqRef.current : undefined,
      );
      const current = openSource(url, {
        open() {
          if (stopped || source !== current) return;
          attempt = 0;
          setStreamState({ runId, state: "live" });
        },
        message(data) {
          if (stopped || source !== current) return;
          let rec: RunEvent;
          try {
            rec = decodeRunEvent(JSON.parse(data), "run_event");
          } catch (error) {
            if (error instanceof SyntaxError || error instanceof DecodeError) return;
            throw error;
          }
          if (rec.seq > lastSeqRef.current) lastSeqRef.current = rec.seq;
          setLiveBucket((prev) => ({
            runId,
            events: mergeRunEvents(prev.runId === runId ? prev.events : [], [rec]),
          }));
          if (rec.kind === "done" || rec.kind === "cancelled") {
            onTerminalRef.current?.();
          }
        },
        error() {
          if (stopped || source !== current) return;
          current.close();
          source = null;
          attempt += 1;
          const delay = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length) - 1] ?? 15_000;
          setStreamState({ runId, state: "reconnecting" });
          retryTimer = setTimeout(() => {
            retryTimer = null;
            connect();
          }, delay);
        },
      });
      source = current;
    };

    connect();

    return () => {
      stopped = true;
      if (retryTimer !== null) clearTimeout(retryTimer);
      source?.close();
    };
  }, [runId, active, openSource]);

  const tailState: TailState = !active
    ? "ended"
    : streamState.runId === runId
      ? streamState.state
      : "connecting";
  return { events: merged, tailState };
}

/**
 * Run Detail screen (`/runs/$id`): metadata header, live SSE event tail (capped 200),
 * steer form, and cancel dialog for active runs.
 */
export function RunDetailScreen({
  openSource = openEventSource,
}: { openSource?: OpenSource } = {}) {
  const { id } = runDetailRoute.useParams();
  const queryClient = useQueryClient();
  const runQuery = useQuery(queries.run(id));
  const eventsQuery = useQuery(queries.runEvents(id));
  const agents = agentIndex(useAgents().data);
  const now = useNow(15_000);

  const runData = runQuery.data;
  useTitle(runData?.title ? `${runData.title} · Run` : `Run ${id}`);

  const active = runData ? isActiveRunState(runData.state) : false;
  const steerable = runData ? isSteerableRunState(runData.state) : false;

  const { events, tailState } = useRunEventTail(
    id,
    active,
    eventsQuery.data,
    () => {
      void invalidateTags(queryClient, [tags.run(id), tags.runs]);
    },
    openSource,
  );

  // Steer form state.
  const [steerText, setSteerText] = useState("");
  const [steerReceipt, setSteerReceipt] = useState<SteerRunResponse | null>(null);
  const steerMutation = useMutation<SteerRunResponse, ApiFailure, string>({
    mutationFn: fetching((message: string) => api.steerRun(id, message)),
    onSuccess(resp) {
      setSteerReceipt(resp);
      setSteerText("");
      void invalidateTags(queryClient, [tags.run(id), tags.runs]);
    },
  });

  // Cancel modal state.
  const [cancelOpen, setCancelOpen] = useState(false);
  const [cancelReason, setCancelReason] = useState("");
  const [cancelReceipt, setCancelReceipt] = useState<CancelRunResponse | null>(null);
  const cancelMutation = useMutation<CancelRunResponse, ApiFailure, string | undefined>({
    mutationFn: fetching((reason?: string) => api.cancelRun(id, reason)),
    onSuccess(resp) {
      setCancelReceipt(resp);
      setCancelOpen(false);
      setCancelReason("");
      void invalidateTags(queryClient, [tags.run(id), tags.runs]);
    },
  });

  return (
    <>
      <Crumbs current={<span className="ident">{id}</span>}>
        <Link to="/runs">Runs</Link>
      </Crumbs>

      <QueryFrame query={runQuery} label="Loading run">
        {(run) => {
          const safeAttach = run.attach_hint ? safeHref(run.attach_hint) : null;
          return (
            <div className="grid gap-6">
              <PageHeader
                title={
                  run.title ? (
                    <PlainText className="whitespace-normal" text={run.title} />
                  ) : (
                    <span className="ident">{run.id}</span>
                  )
                }
                sub={
                  <span className="flex flex-wrap items-center gap-x-2.5 gap-y-1">
                    <span className="ident">{run.id}</span>
                    <span>·</span>
                    <span>
                      Created <RelTime at={run.created_at} />
                    </span>
                    {run.started_at && (
                      <>
                        <span>·</span>
                        <RunElapsed run={run} now={now} />
                      </>
                    )}
                  </span>
                }
                aside={
                  active ? (
                    <Button
                      type="button"
                      variant="destructive"
                      size="sm"
                      onClick={() => {
                        cancelMutation.reset();
                        setCancelOpen(true);
                      }}
                    >
                      Cancel run
                    </Button>
                  ) : undefined
                }
              >
                <RunStatePill state={run.state} />
                <OriginBadge origin={run.origin} />
                <Badge variant="secondary" shape="tag">
                  {run.kind}
                </Badge>
                <Badge variant="outline" shape="tag" className="font-mono">
                  {run.runner}
                </Badge>
                <Badge variant="outline" shape="tag" className="font-mono tabular-nums">
                  {run.cost_units}u
                </Badge>
              </PageHeader>

              {cancelReceipt && (
                <div
                  role="status"
                  className="rounded-lg border border-warn-border bg-warn-soft px-4 py-2.5 text-meta text-warn"
                >
                  {cancelReceipt.wake_id !== null
                    ? `Cancel requested for ${cancelReceipt.run_id} on ${cancelReceipt.device} (wake #${cancelReceipt.wake_id}, state ${cancelReceipt.state}).`
                    : `Cancelled ${cancelReceipt.run_id} on ${cancelReceipt.device} (state ${cancelReceipt.state}).`}
                </div>
              )}

              <section aria-labelledby="run-meta-heading" className="rounded-lg border bg-card p-4">
                <SectionTitle id="run-meta-heading">Execution metadata</SectionTitle>
                <dl className="grid grid-cols-1 gap-x-6 gap-y-3 text-sm sm:grid-cols-2 lg:grid-cols-3">
                  <div>
                    <dt className="text-xs font-medium text-muted-foreground">Device</dt>
                    <dd className="ident mt-0.5 text-foreground">{run.device}</dd>
                  </div>
                  <div>
                    <dt className="text-xs font-medium text-muted-foreground">Runner</dt>
                    <dd className="ident mt-0.5 text-foreground">{run.runner}</dd>
                  </div>
                  <div>
                    <dt className="text-xs font-medium text-muted-foreground">Agent</dt>
                    <dd className="mt-0.5">
                      {run.agent_id ? (
                        <Who id={run.agent_id} agents={agents} />
                      ) : (
                        <span className="text-muted-foreground">Unassigned</span>
                      )}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-xs font-medium text-muted-foreground">Board / Task</dt>
                    <dd className="mt-0.5 flex flex-wrap items-center gap-2">
                      {run.board ? (
                        <Link to="/boards/$slug" params={{ slug: run.board }} className="ident">
                          {run.board}
                        </Link>
                      ) : (
                        <span className="text-muted-foreground">None</span>
                      )}
                      {run.task_id && (
                        <>
                          <span className="text-muted-foreground">·</span>
                          <Link to="/tasks/$id" params={{ id: run.task_id }} className="ident">
                            {run.task_id}
                          </Link>
                        </>
                      )}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-xs font-medium text-muted-foreground">Branch</dt>
                    <dd className="ident mt-0.5 break-all whitespace-normal text-foreground">
                      {run.worktree_branch ?? (
                        <span className="font-sans text-muted-foreground">—</span>
                      )}
                    </dd>
                  </div>
                  <div>
                    <dt className="text-xs font-medium text-muted-foreground">Policy version</dt>
                    <dd className="ident mt-0.5 text-foreground">
                      {run.policy_version ?? (
                        <span className="font-sans text-muted-foreground">—</span>
                      )}
                    </dd>
                  </div>
                  {run.cwd && (
                    <div className="sm:col-span-2 lg:col-span-3">
                      <dt className="text-xs font-medium text-muted-foreground">
                        Working directory
                      </dt>
                      <dd className="mt-0.5 font-mono text-meta break-all text-subtle-foreground">
                        {run.cwd}
                      </dd>
                    </div>
                  )}
                  {run.attach_hint && (
                    <div className="sm:col-span-2 lg:col-span-3">
                      <dt className="text-xs font-medium text-muted-foreground">Attach</dt>
                      <dd className="mt-0.5 flex flex-wrap items-center gap-2">
                        {safeAttach ? (
                          <a
                            href={safeAttach}
                            target="_blank"
                            rel="noopener noreferrer"
                            className="inline-flex items-center gap-1 font-medium"
                          >
                            Open thread <Icon name="external" className="size-3.5" />
                          </a>
                        ) : (
                          <code className="rounded-sm bg-secondary px-1.5 py-0.5 break-all text-subtle-foreground">
                            {run.attach_hint}
                          </code>
                        )}
                      </dd>
                    </div>
                  )}
                  {run.lease_expires_at && active && (
                    <div>
                      <dt className="text-xs font-medium text-muted-foreground">Lease expires</dt>
                      <dd className="mt-0.5 text-meta text-subtle-foreground">
                        <RelTime at={run.lease_expires_at} /> (<AbsTime at={run.lease_expires_at} />
                        )
                      </dd>
                    </div>
                  )}
                  {run.conflict_locks.length > 0 && (
                    <div className="sm:col-span-2">
                      <dt className="text-xs font-medium text-muted-foreground">
                        Held conflict locks
                      </dt>
                      <dd className="mt-1 flex flex-wrap gap-1.5">
                        {run.conflict_locks.map((lock) => (
                          <Badge key={lock.key} variant="outline" shape="tag" className="font-mono">
                            {lock.key} (max {lock.max_holders})
                          </Badge>
                        ))}
                      </dd>
                    </div>
                  )}
                  {run.transcript_uri && (
                    <div className="sm:col-span-2 lg:col-span-3">
                      <dt className="text-xs font-medium text-muted-foreground">
                        Archived transcript
                      </dt>
                      <dd className="mt-0.5 grid gap-0.5">
                        <code className="text-meta break-all text-foreground">
                          {run.transcript_uri}
                        </code>
                        {run.transcript_sha256 && (
                          <span className="font-mono text-xs break-all text-muted-foreground">
                            sha256: {run.transcript_sha256}
                          </span>
                        )}
                      </dd>
                    </div>
                  )}
                </dl>
              </section>

              {steerable && (
                <section
                  aria-labelledby="run-steer-heading"
                  className="rounded-lg border bg-card p-4"
                >
                  <SectionTitle id="run-steer-heading">Steer run</SectionTitle>
                  <p className="mb-3 text-meta text-muted-foreground">
                    Enqueues a sticky message for <span className="ident">{run.device}</span> to
                    deliver directly to the active runner session.
                  </p>
                  {steerMutation.error && <ErrorNotice error={steerMutation.error} compact />}
                  {steerReceipt && (
                    <div
                      role="status"
                      className="mb-3 rounded-md border border-status-ready/35 bg-status-ready-soft px-3 py-2 text-meta text-status-ready"
                    >
                      Delivered to sticky queue on{" "}
                      <span className="ident">{steerReceipt.device}</span> (wake #
                      {steerReceipt.wake_id}).
                    </div>
                  )}
                  <form
                    className="grid gap-2.5"
                    onSubmit={(event) => {
                      event.preventDefault();
                      const trimmed = steerText.trim();
                      if (!trimmed || steerMutation.isPending) return;
                      setSteerReceipt(null);
                      steerMutation.mutate(trimmed);
                    }}
                  >
                    <label htmlFor="steer-message" className="sr-only">
                      Steering message
                    </label>
                    <Textarea
                      id="steer-message"
                      rows={2}
                      maxLength={4000}
                      value={steerText}
                      onChange={(event) => setSteerText(event.currentTarget.value)}
                      placeholder="Send instructions or a course correction to this active run…"
                      disabled={steerMutation.isPending}
                    />
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-xs text-muted-foreground tabular-nums">
                        {steerText.trim().length} / 4000
                      </span>
                      <Button
                        type="submit"
                        size="sm"
                        disabled={!steerText.trim() || steerMutation.isPending}
                      >
                        {steerMutation.isPending ? "Sending…" : "Send steer"}
                      </Button>
                    </div>
                  </form>
                </section>
              )}

              <section aria-labelledby="run-events-heading" className="grid gap-2.5">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <SectionTitle id="run-events-heading" count={events.length}>
                    Live event tail
                  </SectionTitle>
                  <span
                    className={cn(
                      "inline-flex items-center gap-1.5 text-xs font-medium",
                      tailState === "live"
                        ? "text-status-ready"
                        : tailState === "reconnecting" || tailState === "connecting"
                          ? "text-amber"
                          : "text-muted-foreground",
                    )}
                    role="status"
                  >
                    <span
                      className={cn(
                        "size-1.75 rounded-full",
                        tailState === "live" && "bg-status-ready",
                        (tailState === "connecting" || tailState === "reconnecting") && "bg-amber",
                        (tailState === "ended" || tailState === "idle") && "bg-status-done",
                      )}
                      aria-hidden="true"
                    />
                    {tailState === "live"
                      ? "Live SSE tail (max 200)"
                      : tailState === "connecting"
                        ? "Connecting to stream…"
                        : tailState === "reconnecting"
                          ? "Reconnecting…"
                          : "Run ended"}
                  </span>
                </div>

                {eventsQuery.error && (
                  <ErrorNotice
                    error={eventsQuery.error}
                    retry={() => void eventsQuery.refetch()}
                    compact
                  />
                )}

                {events.length === 0 && !eventsQuery.isLoading ? (
                  <Empty icon="events" title="No live events yet">
                    {run.transcript_uri
                      ? "Live events were pruned after archival. See the archived transcript URI above."
                      : "Structured runner milestones (phase, tool, note, pr, error, done) stream here as they occur."}
                  </Empty>
                ) : (
                  <ol className="divide-y overflow-hidden rounded-lg border bg-card">
                    {events.map((ev) => (
                      <li
                        key={ev.seq}
                        className="grid grid-cols-[52px_84px_minmax(0,1fr)_auto] items-baseline gap-x-3 gap-y-1 px-3.5 py-2 text-sm max-md:grid-cols-[44px_minmax(0,1fr)]"
                      >
                        <span className="ident text-xs text-muted-foreground tabular-nums">
                          #{ev.seq}
                        </span>
                        <div>
                          <Badge variant={eventKindVariant(ev.kind)} shape="tag">
                            {ev.kind}
                          </Badge>
                        </div>
                        <div className="min-w-0 max-md:col-span-2">
                          <PlainText className="whitespace-normal" text={ev.summary} />
                          {ev.meta !== null && (
                            <details className="mt-1">
                              <summary className="cursor-pointer text-xs text-muted-foreground hover:text-foreground">
                                Metadata
                              </summary>
                              <pre className="mt-1 overflow-x-auto rounded-sm border bg-muted px-2.5 py-1.5 text-xs text-subtle-foreground">
                                {JSON.stringify(ev.meta, null, 2)}
                              </pre>
                            </details>
                          )}
                        </div>
                        <span className="text-xs whitespace-nowrap text-muted-foreground tabular-nums max-md:col-start-2 max-md:justify-self-end">
                          <RelTime at={ev.at} />
                        </span>
                      </li>
                    ))}
                  </ol>
                )}
              </section>

              <Dialog
                open={cancelOpen}
                onOpenChange={(next) => !next && setCancelOpen(false)}
                aria-labelledby="cancel-run-title"
              >
                <form
                  method="dialog"
                  className="grid gap-4"
                  onSubmit={(event) => {
                    event.preventDefault();
                    if (cancelMutation.isPending) return;
                    const trimmed = cancelReason.trim();
                    cancelMutation.mutate(trimmed ? trimmed : undefined);
                  }}
                >
                  <DialogHeader>
                    <DialogTitle id="cancel-run-title">Cancel run {run.id}?</DialogTitle>
                    <DialogDescription>
                      {run.state === "queued"
                        ? "This queued run will transition immediately to cancelled and release any held locks."
                        : `Queues a cancel command on ${run.device} to stop the active runner and record a Trail Rule event.`}
                    </DialogDescription>
                  </DialogHeader>
                  {cancelMutation.error && <ErrorNotice error={cancelMutation.error} compact />}
                  <div className="grid gap-1.5">
                    <label
                      htmlFor="cancel-reason"
                      className="text-meta font-medium text-foreground"
                    >
                      Reason (optional)
                    </label>
                    <Input
                      id="cancel-reason"
                      maxLength={500}
                      value={cancelReason}
                      onChange={(event) => setCancelReason(event.currentTarget.value)}
                      placeholder="e.g. superseded by another change"
                      disabled={cancelMutation.isPending}
                    />
                  </div>
                  <DialogFooter>
                    <Button
                      type="button"
                      variant="outline"
                      onClick={() => setCancelOpen(false)}
                      disabled={cancelMutation.isPending}
                    >
                      Keep running
                    </Button>
                    <Button type="submit" variant="destructive" disabled={cancelMutation.isPending}>
                      {cancelMutation.isPending ? "Cancelling…" : "Cancel run"}
                    </Button>
                  </DialogFooter>
                </form>
              </Dialog>
            </div>
          );
        }}
      </QueryFrame>
    </>
  );
}
