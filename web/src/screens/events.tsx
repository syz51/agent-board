import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useMemo, useState } from "react";
import { api } from "@/api/client.ts";
import type { Agent, BoardEvent, EventsPage } from "@/api/schema.ts";
import { Button } from "@/components/ui/button.tsx";
import { cn } from "@/lib/utils.ts";
import { EVENTS_PAGE, fetching, mergeEvents, queries } from "@/live/query.ts";
import {
  agentIndex,
  Empty,
  ErrorNotice,
  PageHeader,
  QueryFrame,
  RelTime,
  useAgents,
  useTitle,
  Who,
} from "@/ui/common.tsx";
import { Icon, type IconName, StatusIcon, type StatusKey } from "@/ui/icons.tsx";
import { FilterSelect, FilterToggle } from "@/ui/filters.tsx";
import { NativeSelectOption } from "@/components/ui/native-select.tsx";

const NOISY = new Set(["lease_created", "lease_released", "lease_expired", "files_touched"]);

export function EventsScreen() {
  useTitle("Events");
  const client = useQueryClient();
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);
  const boards = useQuery(queries.boards());
  const events = useQuery(queries.events());
  const [board, setBoard] = useState("");
  const [showNoisy, setShowNoisy] = useState(false);

  const oldest = events.data?.at(-1)?.seq ?? 0;
  const older = useMutation({
    mutationFn: fetching(() =>
      api.events({ since: Math.max(0, oldest - 1 - EVENTS_PAGE), limit: EVENTS_PAGE }),
    ),
    onSuccess: (page: EventsPage) =>
      client.setQueryData(queries.events().queryKey, (current) =>
        mergeEvents(current ?? [], page.events),
      ),
  });

  return (
    <>
      <PageHeader title="Events" sub="Everything agents and you did, live as it happens." />
      <form
        className="mb-4 flex flex-wrap items-center gap-2"
        onSubmit={(event) => event.preventDefault()}
      >
        <FilterSelect label="Board" value={board} onChange={setBoard}>
          <NativeSelectOption value="">All boards</NativeSelectOption>
          {(boards.data ?? []).map((item) => (
            <NativeSelectOption key={item.slug} value={item.slug}>
              {item.title}
            </NativeSelectOption>
          ))}
        </FilterSelect>
        <FilterToggle checked={showNoisy} onChange={setShowNoisy}>
          Show leases and file edits
        </FilterToggle>
      </form>

      {older.error && <ErrorNotice error={older.error} retry={() => older.mutate()} compact />}
      <QueryFrame query={events} label="Loading events">
        {(list) => {
          const shown = list.filter(
            (event) => (!board || event.board === board) && (showNoisy || !NOISY.has(event.type)),
          );
          return (
            <>
              {shown.length === 0 ? (
                <Empty title="No events yet" icon="events">
                  New events appear here as they happen.
                </Empty>
              ) : (
                <Timeline live>
                  {shown.map((event) => (
                    <EventLine key={event.seq} event={event} agents={agents} showTask />
                  ))}
                </Timeline>
              )}
              {oldest > 1 && (
                <div className="mt-4 flex justify-center">
                  <Button
                    type="button"
                    variant="outline"
                    disabled={older.isPending}
                    onClick={() => older.mutate()}
                  >
                    {older.isPending ? "Loading…" : "Load older events"}
                  </Button>
                </div>
              )}
            </>
          );
        }}
      </QueryFrame>
    </>
  );
}

/** A log of events: one ruled, striped table row per event. */
export function Timeline({ live, children }: { live?: boolean; children: React.ReactNode }) {
  return (
    <ol
      className="divide-y overflow-hidden rounded-md border bg-card"
      {...(live ? { "aria-live": "polite" as const, "aria-relevant": "additions" as const } : {})}
    >
      {children}
    </ol>
  );
}

const KIND_ICON: Record<string, IconName> = {
  task: "task",
  ask: "question",
  agent: "agent",
  evidence: "shield",
  handoff: "handoff",
  note: "note",
  board: "board",
  lease: "lock",
  files: "file",
};

/** Events about the agent itself, where the board adds nothing. */
const PERSONAL = new Set(["handoff_updated", "agent_state_changed"]);

const TASK_STATUS: Record<string, StatusKey> = {
  open: "open",
  doing: "doing",
  blocked: "blocked",
  review: "review",
  done: "done",
  dropped: "dropped",
};

/**
 * One rule for every event's mark: it shows the state the event leaves behind. Task events
 * show the task's new status with the board's status symbol; asks show their kind; evidence
 * shows a pass or a failure; everything else shows a grey symbol of what it is about.
 * Exported for tests.
 */
export function eventMark(event: BoardEvent): {
  status?: StatusKey;
  icon?: IconName;
  tone: string;
} {
  const payload = (event.payload ?? {}) as Record<string, unknown>;
  const to = typeof payload.to === "string" ? TASK_STATUS[payload.to] : undefined;
  switch (event.type) {
    case "task_created":
      return { status: "open", tone: "" };
    case "task_claimed":
      return { status: "doing", tone: "" };
    case "task_done":
      return { status: "done", tone: "" };
    case "task_updated":
      return to ? { status: to, tone: "" } : { icon: "task", tone: "text-muted-foreground" };
    case "ask_created":
      return payload.kind === "action"
        ? { icon: "hand", tone: "text-amber" }
        : { icon: "question", tone: "text-status-review" };
    case "ask_answered":
      return { icon: "check", tone: "text-status-ready" };
    case "evidence_added":
      return typeof payload.exit_code === "number" && payload.exit_code !== 0
        ? { icon: "cross", tone: "text-status-blocked" }
        : { icon: "shield", tone: "text-muted-foreground" };
    default:
      return {
        icon: KIND_ICON[event.type.split("_")[0] ?? ""] ?? "events",
        tone: "text-muted-foreground",
      };
  }
}

/** The mark itself: a status symbol or an icon, on the same neutral node for every event. */
export function EventMark({ event, className }: { event: BoardEvent; className?: string }) {
  const mark = eventMark(event);
  return (
    <span
      className={cn(
        "grid size-5 flex-none place-items-center rounded-sm border bg-card",
        mark.tone,
        className,
      )}
      aria-hidden="true"
    >
      {mark.status ? (
        <StatusIcon status={mark.status} className="size-3.5" />
      ) : (
        <Icon name={mark.icon ?? "events"} className="size-3.5" />
      )}
    </span>
  );
}

export function EventLine({
  event,
  agents,
  showTask,
}: {
  event: BoardEvent;
  agents: Map<string, Agent>;
  showTask: boolean;
}) {
  const summary = detail(event);
  const [verb, suffix] = describe(event);
  const kind = event.type.split("_")[0] ?? "";
  return (
    <li
      className="grid grid-cols-[20px_minmax(0,1fr)_auto] items-start gap-x-3 px-3 py-1.5 even:bg-muted/60 max-[520px]:grid-cols-[20px_minmax(0,1fr)]"
      data-kind={kind}
    >
      <EventMark event={event} className="mt-px" />
      <div className="grid min-w-0 gap-0.5 pt-0.5">
        <p className="flex flex-wrap items-center gap-x-1.5 gap-y-0.5 text-foreground">
          <Who id={event.actor} agents={agents} />
          <span title={event.type}>{verb}</span>
          {showTask && event.task && (
            <Link to="/tasks/$id" params={{ id: event.task }}>
              <span className="ident text-meta">{event.task}</span>
            </Link>
          )}
          {showTask && !event.task && event.board && !PERSONAL.has(event.type) && (
            <Link to="/boards/$slug" params={{ slug: event.board }}>
              <span className="ident text-meta">{event.board}</span>
            </Link>
          )}
          {suffix && <span>{suffix}</span>}
        </p>
        {summary && (
          <p className="text-meta [overflow-wrap:anywhere] text-muted-foreground">{summary}</p>
        )}
      </div>
      <span className="inline-flex gap-2 pt-0.75 font-mono text-xs whitespace-nowrap text-muted-foreground tabular-nums max-[520px]:col-start-2 max-[520px]:pt-0">
        <RelTime at={event.at} />
        <span className="min-w-[5ch] text-right [font-variant-numeric:tabular-nums_slashed-zero]">
          #{event.seq}
        </span>
      </span>
    </li>
  );
}

/** What happened, as a verb before the task or board and an optional ending after it. */
export function describe(event: BoardEvent): [string, string] {
  const payload = (event.payload ?? {}) as Record<string, unknown>;
  const to = typeof payload.to === "string" ? payload.to : "";
  switch (event.type) {
    case "task_created":
      return ["created", ""];
    case "task_updated":
      return to ? ["moved", `to ${to}`] : ["updated", ""];
    case "task_done":
      return ["finished", ""];
    case "task_claimed":
      return ["claimed", ""];
    case "evidence_added":
      return ["added evidence to", ""];
    case "note_added":
      return [`added a ${typeof payload.kind === "string" ? payload.kind : "note"} to`, ""];
    case "ask_created":
      return [payload.kind === "action" ? "asked you to act on" : "asked you to decide on", ""];
    case "ask_answered":
      return ["answered an ask on", ""];
    case "handoff_updated":
      return ["updated their handoff", ""];
    case "agent_state_changed":
      return [to ? `went ${to}` : "changed state", ""];
    case "board_updated":
      return ["updated", ""];
    case "lease_created":
      return ["leased files for", ""];
    case "files_touched":
      return ["edited files for", ""];
    default:
      return [event.type.replaceAll("_", " "), ""];
  }
}

/** The payload's details the sentence does not already say. */
export function detail(event: BoardEvent): string {
  const payload = event.payload;
  if (payload === null || typeof payload !== "object" || Array.isArray(payload)) {
    return summarize(payload);
  }
  const record = { ...(payload as Record<string, unknown>) };
  if (event.type === "ask_created" && typeof record.ask === "number") return `Ask #${record.ask}`;
  if (event.type === "evidence_added") {
    const parts = [typeof record.kind === "string" ? record.kind : "evidence"];
    if (typeof record.exit_code === "number") parts.push(`exit ${record.exit_code}`);
    return parts.join(" · ");
  }
  if (event.type === "note_added") delete record.kind;
  if (["task_updated", "task_done", "agent_state_changed", "task_claimed"].includes(event.type)) {
    delete record.from;
    delete record.to;
  }
  return summarize(record);
}

/** A one-line text view of an event's payload; values written by agents stay text. */
export function summarize(payload: unknown): string {
  if (payload === null || payload === undefined) return "";
  if (typeof payload !== "object") return String(payload);
  if (Array.isArray(payload)) return payload.map(summarize).join(", ");
  const record = payload as Record<string, unknown>;
  if (typeof record.from === "string" && typeof record.to === "string") {
    const rest = Object.entries(record).filter(([key]) => key !== "from" && key !== "to");
    const extra = rest.length ? ` · ${rest.map(([k, v]) => `${k}: ${short(v)}`).join(" · ")}` : "";
    return `${record.from} → ${record.to}${extra}`;
  }
  return Object.entries(record)
    .map(([key, value]) => `${key}: ${short(value)}`)
    .join(" · ");
}

function short(value: unknown): string {
  const text = typeof value === "string" ? value : (JSON.stringify(value) ?? "");
  return text.length > 120 ? `${text.slice(0, 119)}…` : text;
}
