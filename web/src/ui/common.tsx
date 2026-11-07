import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { type ReactNode, useEffect } from "react";
import type { ApiFailure } from "@/api/client.ts";
import type { Agent, Priority, Task, TaskStatus } from "@/api/schema.ts";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert.tsx";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import { cn } from "@/lib/utils.ts";
import { queries } from "@/live/query.ts";
import { useNow } from "@/live/runtime.ts";
import { Avatar, Icon, type IconName, PriorityMark, StatusPill } from "./icons.tsx";

export function useTitle(title: string): void {
  useEffect(() => {
    document.title = title ? `${title} · agent-board` : "agent-board";
  }, [title]);
}

export function Loading({ label = "Loading" }: { label?: string }) {
  return (
    <div
      className="flex items-center justify-center gap-2.5 px-6 py-12 text-muted-foreground"
      role="status"
    >
      <span
        className="size-4 animate-spin rounded-full border-2 border-input border-t-primary motion-reduce:animate-[spin_2.4s_linear_infinite]"
        aria-hidden="true"
      />
      {label}…
    </div>
  );
}

export function ErrorNotice({
  error,
  retry,
  compact,
}: {
  error: ApiFailure;
  retry?: (() => void) | undefined;
  compact?: boolean;
}) {
  return (
    <Alert
      variant="error"
      className={cn(
        "mb-5 flex flex-wrap items-start gap-x-4 gap-y-3 has-[>svg]:grid-cols-none",
        compact ? "px-3.5 py-2.5" : "px-4.5 py-4",
      )}
    >
      {!compact && (
        <span className="grid size-9 flex-none place-items-center rounded-[9px] border border-error-border bg-card">
          <Icon name="plug" className="size-4.5" />
        </span>
      )}
      <div className="grid flex-[1_1_280px] gap-1">
        <AlertTitle className="col-start-1 line-clamp-none font-semibold">
          {error.message}
        </AlertTitle>
        <AlertDescription className="col-start-1 text-error">
          {error.next && <p>{error.next}</p>}
          <p className="text-xs opacity-85">
            {error.status > 0 ? `HTTP ${error.status} · ` : ""}
            <code>{error.code}</code>
          </p>
        </AlertDescription>
      </div>
      {retry && (
        <Button type="button" variant="outline" onClick={retry}>
          Try again
        </Button>
      )}
    </Alert>
  );
}

export function Empty({
  title,
  children,
  icon = "boards",
}: {
  title: string;
  children?: ReactNode;
  icon?: IconName;
}) {
  return (
    <div className="grid justify-items-center gap-1.5 rounded-lg border border-dashed border-input bg-muted px-6 py-12 text-center text-muted-foreground">
      <span className="mb-1.5 grid size-10 place-items-center rounded-[10px] border bg-card text-subtle-foreground shadow-raise">
        <Icon name={icon} className="size-5" />
      </span>
      <p className="text-base font-semibold text-foreground">{title}</p>
      {children && <p className="max-w-[48ch]">{children}</p>}
    </div>
  );
}

/**
 * The standard frame for a query: loading on first load, the error when there is no data,
 * and an inline error above stale data when a refresh failed.
 */
export function QueryFrame<T>({
  query,
  label,
  children,
}: {
  query: UseQueryResult<T, ApiFailure>;
  label?: string;
  children: (data: T) => ReactNode;
}) {
  const retry = () => void query.refetch();
  if (query.data === undefined) {
    if (query.error) return <ErrorNotice error={query.error} retry={retry} />;
    return <Loading {...(label ? { label } : {})} />;
  }
  return (
    <>
      {query.error && <ErrorNotice error={query.error} retry={retry} compact />}
      {children(query.data)}
    </>
  );
}

const timeOnly = new Intl.DateTimeFormat(undefined, { timeStyle: "short" });
const dayAndTime = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  hour: "numeric",
  minute: "2-digit",
});

/** A clock time for a moment soon, with the date when it is not today. */
export function formatUntil(at: string, now: number): string {
  const when = new Date(at);
  return when.toDateString() === new Date(now).toDateString()
    ? timeOnly.format(when)
    : dayAndTime.format(when);
}

const relative = new Intl.RelativeTimeFormat(undefined, { numeric: "auto", style: "narrow" });
const absolute = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });

export function formatRelative(at: string, now: number): string {
  const seconds = Math.round((Date.parse(at) - now) / 1000);
  const abs = Math.abs(seconds);
  if (abs < 45) return "just now";
  if (abs < 3600) return relative.format(Math.round(seconds / 60), "minute");
  if (abs < 86_400) return relative.format(Math.round(seconds / 3600), "hour");
  return relative.format(Math.round(seconds / 86_400), "day");
}

export function formatAbsolute(at: string): string {
  return absolute.format(new Date(at));
}

/** An absolute time, for a moment that matters by its date. */
export function AbsTime({ at }: { at: string }) {
  return <time dateTime={at}>{formatAbsolute(at)}</time>;
}

/** A relative time; the absolute time is in `title` and `datetime`. */
export function RelTime({ at }: { at: string }) {
  const now = useNow();
  return (
    <time dateTime={at} title={formatAbsolute(at)}>
      {formatRelative(at, now)}
    </time>
  );
}

export function StatusBadge({ status, ready }: { status: TaskStatus; ready?: boolean }) {
  return <StatusPill status={status === "open" && ready ? "ready" : status} />;
}

export function PriorityBadge({ priority, compact }: { priority: Priority; compact?: boolean }) {
  return <PriorityMark priority={priority} {...(compact ? { compact } : {})} />;
}

/** A state as a dot whose shape says it: filled running, bars paused, dotted ended. */
export function StateDot({ state }: { state: "running" | "paused" | "idle" | "ended" | "closed" }) {
  return (
    <span
      className={cn(
        "size-1.75 flex-none rounded-full border-[1.5px] border-current",
        state === "running" && "bg-current",
        state === "paused" && "w-1.5 rounded-[1px] border-x-2 border-y-0",
        (state === "ended" || state === "closed") && "border-dotted",
      )}
      aria-hidden="true"
    />
  );
}

/** A pill with a state dot: agent and board states. */
export function StatePill({
  state,
  children,
}: {
  state: "running" | "paused" | "idle" | "ended" | "closed";
  children: ReactNode;
}) {
  return (
    <span
      className={cn(
        "inline-flex h-5.5 items-center gap-1.5 rounded-full pr-2.25 pl-1.75 text-xs font-[550] whitespace-nowrap",
        state === "running"
          ? "bg-status-ready-soft text-status-ready"
          : state === "paused"
            ? "bg-amber-soft text-amber"
            : "bg-status-open-soft text-status-open",
      )}
    >
      <StateDot state={state} />
      {children}
    </span>
  );
}

export function AgentStateBadge({ agent }: { agent: Pick<Agent, "state" | "paused_until"> }) {
  const now = useNow();
  return (
    <StatePill state={agent.state}>
      {agent.state === "paused" && agent.paused_until ? (
        <>
          Paused until{" "}
          <time dateTime={agent.paused_until} title={formatRelative(agent.paused_until, now)}>
            {formatUntil(agent.paused_until, now)}
          </time>
        </>
      ) : (
        agent.state[0]?.toUpperCase() + agent.state.slice(1)
      )}
    </StatePill>
  );
}

/** An owner's handle, flagged when stale on work that is still open. */
export function Owner({
  task,
  agents,
}: {
  task: Pick<Task, "owner" | "owner_stale" | "status">;
  agents: Map<string, Agent>;
}) {
  if (!task.owner) return <span className="text-meta text-muted-foreground">Unowned</span>;
  const closed = task.status === "done" || task.status === "dropped";
  return (
    <span className="inline-flex min-w-0 items-center gap-2">
      <Who id={task.owner} agents={agents} />
      {task.owner_stale && !closed && (
        <Badge
          variant="warn"
          shape="tag"
          title="Not seen for 24 hours; another agent may take this task"
        >
          stale
        </Badge>
      )}
    </span>
  );
}

export function useAgents() {
  return useQuery(queries.agents());
}

/** Agents by id, for showing handles in place of full ids. */
export function agentIndex(agents: Agent[] | undefined): Map<string, Agent> {
  return new Map((agents ?? []).map((agent) => [agent.id, agent]));
}

/** An agent's handle, or the id when the agent is unknown; `user` as "you". */
export function Who({ id, agents }: { id: string; agents: Map<string, Agent> }) {
  const agent = agents.get(id);
  if (id === "user") return <span className="font-[550] text-foreground">you</span>;
  const handle = agent ? agent.handle : id;
  return (
    <span className="inline-flex min-w-0 items-center gap-1.5 text-subtle-foreground" title={id}>
      <Avatar handle={handle} />
      <span className="truncate text-meta font-medium tabular-nums">{handle}</span>
    </span>
  );
}

/** A page's title block: the h1, an optional line under it, and actions on the right. */
export function PageHeader({
  title,
  sub,
  aside,
  children,
}: {
  title: ReactNode;
  sub?: ReactNode;
  aside?: ReactNode;
  /** Shown beside the title, such as the slug and tags. */
  children?: ReactNode;
}) {
  return (
    <header className="mb-6 flex flex-wrap items-start justify-between gap-x-4 gap-y-3">
      <div className="grid min-w-0 gap-1.5">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5">
          <h1 className="text-display font-[650] tracking-[-0.022em] text-balance max-md:text-display-phone">
            {title}
          </h1>
          {children}
        </div>
        {sub && <p className="max-w-[68ch] text-sm text-muted-foreground">{sub}</p>}
      </div>
      {aside}
    </header>
  );
}

/** The breadcrumb trail above a page title; the last item is the current page. */
export function Crumbs({ children, current }: { children: ReactNode; current: ReactNode }) {
  return (
    <nav
      className="mb-3 flex flex-wrap items-center gap-1.5 text-meta text-muted-foreground [&_a]:text-muted-foreground [&_a:hover]:text-foreground"
      aria-label="Breadcrumb"
    >
      {children}
      <Icon name="chevron" className="size-3 opacity-70" />
      <span className="text-subtle-foreground" aria-current="page">
        {current}
      </span>
    </nav>
  );
}

/** A section heading (h2) with an optional count beside it. */
export function SectionTitle({
  id,
  count,
  icon,
  children,
}: {
  id: string;
  count?: number;
  icon?: ReactNode;
  children: ReactNode;
}) {
  return (
    <h2
      id={id}
      className="mb-3 flex items-baseline gap-2 text-base font-semibold tracking-[-0.011em]"
    >
      {icon}
      {children}
      {count !== undefined && (
        <span className="text-meta font-medium tracking-normal text-muted-foreground tabular-nums">
          {count}
        </span>
      )}
    </h2>
  );
}

/** A bordered list of rows on a card surface. */
export const listClass =
  "bg-card shadow-raise divide-y overflow-hidden rounded-lg border [&>li]:border-border";

/** An ask's kind as an icon and a word: actions in amber, decisions in violet. */
export function AskKind({ kind }: { kind: "action" | "decision" }) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.5 text-xs font-semibold whitespace-nowrap",
        kind === "action" ? "text-amber" : "text-status-review",
      )}
    >
      <Icon name={kind === "action" ? "hand" : "question"} className="size-3.5" />
      {kind === "action" ? "Action" : "Decision"}
    </span>
  );
}
