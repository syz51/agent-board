import type { ComponentChildren } from "preact";
import { useEffect } from "preact/hooks";
import type { ApiFailure } from "../api/client.ts";
import { api } from "../api/client.ts";
import type { Agent, Priority, Task, TaskStatus } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { type Resource, useNow, useResource } from "../live/runtime.ts";

export function useTitle(title: string): void {
  useEffect(() => {
    document.title = title ? `${title} · agent-board` : "agent-board";
  }, [title]);
}

export function Loading({ label = "Loading" }: { label?: string }) {
  return (
    <div class="state state-loading" role="status">
      <span class="spinner" aria-hidden="true" />
      {label}…
    </div>
  );
}

export function ErrorNotice(
  { error, retry, compact }: { error: ApiFailure; retry?: () => void; compact?: boolean },
) {
  return (
    <div class={compact ? "notice notice-error compact" : "notice notice-error"} role="alert">
      <div class="notice-body">
        <p class="notice-title">{error.message}</p>
        {error.next && <p class="notice-next">{error.next}</p>}
        <p class="notice-code">
          {error.status > 0 ? `HTTP ${error.status} · ` : ""}
          <code>{error.code}</code>
        </p>
      </div>
      {retry && <button type="button" class="button" onClick={retry}>Try again</button>}
    </div>
  );
}

export function Empty({ title, children }: { title: string; children?: ComponentChildren }) {
  return (
    <div class="state state-empty">
      <p class="state-title">{title}</p>
      {children && <p class="state-detail">{children}</p>}
    </div>
  );
}

/**
 * The standard frame for a query: loading on first load, the error when there is no data,
 * and an inline error above stale data when a refresh failed.
 */
export function Query<T>(
  { resource, label, children }: {
    resource: Resource<T>;
    label?: string;
    children: (data: T) => ComponentChildren;
  },
) {
  if (resource.data === undefined) {
    if (resource.error) return <ErrorNotice error={resource.error} retry={resource.retry} />;
    return <Loading {...(label ? { label } : {})} />;
  }
  return (
    <>
      {resource.error && <ErrorNotice error={resource.error} retry={resource.retry} compact />}
      {children(resource.data)}
    </>
  );
}

const timeOnly = new Intl.DateTimeFormat(undefined, { timeStyle: "short" });
const dayAndTime = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });

/** A clock time for a moment soon, with the date when it is not today. */
export function formatUntil(at: string, now: number): string {
  const when = new Date(at);
  return when.toDateString() === new Date(now).toDateString() ? timeOnly.format(when) : dayAndTime.format(when);
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

const STATUS_LABEL: Record<TaskStatus, string> = {
  open: "Open",
  doing: "Doing",
  blocked: "Blocked",
  review: "Review",
  done: "Done",
  dropped: "Dropped",
};

export function StatusBadge({ status, ready }: { status: TaskStatus; ready?: boolean }) {
  if (status === "open" && ready) return <span class="badge tone-ready">Ready</span>;
  return <span class={`badge tone-${status}`}>{STATUS_LABEL[status]}</span>;
}

export function PriorityBadge({ priority }: { priority: Priority }) {
  return <span class={`prio prio-${priority}`}>{priority.toUpperCase()}</span>;
}

const AGENT_STATE_TONE: Record<Agent["state"], string> = {
  running: "tone-doing",
  idle: "tone-open",
  paused: "tone-blocked",
  ended: "tone-dropped",
};

export function AgentStateBadge({ agent }: { agent: Pick<Agent, "state" | "paused_until"> }) {
  const now = useNow();
  return (
    <span class={`badge ${AGENT_STATE_TONE[agent.state]}`}>
      {agent.state === "paused" && agent.paused_until
        ? (
          <>
            Paused until{" "}
            <time dateTime={agent.paused_until} title={formatRelative(agent.paused_until, now)}>
              {formatUntil(agent.paused_until, now)}
            </time>
          </>
        )
        : agent.state[0]?.toUpperCase() + agent.state.slice(1)}
    </span>
  );
}

/** An owner's handle, flagged when stale on work that is still open. */
export function Owner({ task, agents }: { task: Pick<Task, "owner" | "owner_stale" | "status">; agents: Map<string, Agent> }) {
  if (!task.owner) return <span class="muted">unowned</span>;
  const closed = task.status === "done" || task.status === "dropped";
  return (
    <span class="owner">
      <Who id={task.owner} agents={agents} />
      {task.owner_stale && !closed && (
        <span class="badge tone-blocked" title="Not seen for 24 hours; another agent may take this task">stale</span>
      )}
    </span>
  );
}

export function useAgents(): Resource<Agent[]> {
  return useResource("agents", [tags.agents], () => api.agents());
}

/** Agents by id, for showing handles in place of full ids. */
export function agentIndex(agents: Agent[] | undefined): Map<string, Agent> {
  return new Map((agents ?? []).map((agent) => [agent.id, agent]));
}

/** An agent's handle, or the id when the agent is unknown; `user` and `server` as is. */
export function Who({ id, agents }: { id: string; agents: Map<string, Agent> }) {
  const agent = agents.get(id);
  return <span class="handle" title={id}>{agent ? agent.handle : id}</span>;
}

/** `factory-12` → `factory`. */
export function boardOfTask(taskId: string): string {
  return taskId.replace(/-\d+$/, "");
}
