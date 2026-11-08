// A task's history on its page: one line per event, joined by a rule through their marks.

import type { Agent, BoardEvent } from "@/api/schema.ts";
import { cn } from "@/lib/utils.ts";
import { RelTime, Who } from "@/ui/common.tsx";
import { Icon, type IconName, STATUS_COLOR, StatusIcon, type StatusKey } from "@/ui/icons.tsx";

/** A vertical timeline of events, joined by a rule through their icons. */
export function Timeline({ children }: { children: React.ReactNode }) {
  return (
    <ol className="relative before:absolute before:top-3.5 before:bottom-3.5 before:left-[13px] before:w-px before:bg-input">
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
    case "task_taken": {
      // Taking a task keeps its status, or moves an open one to doing; the payload says which.
      const status = typeof payload.status === "string" ? TASK_STATUS[payload.status] : undefined;
      return { status: status ?? "doing", tone: "" };
    }
    case "task_released":
      return { status: "open", tone: "" };
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
        : { icon: "shield", tone: "text-subtle-foreground" };
    case "agent_state_changed":
      // The agent's new state, with the shape its state pill uses.
      return payload.to === "paused"
        ? { icon: "pause", tone: "text-amber" }
        : payload.to === "idle"
          ? { icon: "idle", tone: "text-subtle-foreground" }
          : payload.to === "running"
            ? { icon: "agent", tone: "text-status-ready" }
            : { icon: "agent", tone: "text-subtle-foreground" };
    default:
      return {
        icon: KIND_ICON[event.type.split("_")[0] ?? ""] ?? "events",
        tone: "text-subtle-foreground",
      };
  }
}

/** The mark itself: a status symbol or an icon on a node tinted with the mark's own color. */
function EventMark({ event, className }: { event: BoardEvent; className?: string }) {
  const mark = eventMark(event);
  return (
    <span
      className={cn(
        // Opaque, so the rail stops at the node; the tint and ring come from the text color.
        "grid size-6 flex-none place-items-center rounded-full border border-current/25 bg-[color-mix(in_oklab,currentColor_11%,var(--color-card))]",
        mark.status ? STATUS_COLOR[mark.status] : mark.tone,
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

export function EventLine({ event, agents }: { event: BoardEvent; agents: Map<string, Agent> }) {
  const summary = detail(event);
  const [verb, suffix] = describe(event);
  const kind = event.type.split("_")[0] ?? "";
  return (
    <li
      className="relative grid grid-cols-[28px_minmax(0,1fr)_auto] items-start gap-x-3 py-1.75 max-[520px]:grid-cols-[28px_minmax(0,1fr)]"
      data-kind={kind}
    >
      <EventMark event={event} className="m-0.5" />
      <div className="grid min-w-0 gap-0.5 pt-1">
        <p className="flex flex-wrap items-center gap-x-1.5 gap-y-0.5 text-foreground">
          <Who id={event.actor} agents={agents} />
          <span title={event.type}>{verb}</span>
          {suffix && <span>{suffix}</span>}
        </p>
        {summary && (
          <p className="text-meta [overflow-wrap:anywhere] text-muted-foreground">{summary}</p>
        )}
      </div>
      <span className="inline-flex gap-2 pt-1.25 text-meta whitespace-nowrap text-muted-foreground tabular-nums max-[520px]:col-start-2 max-[520px]:pt-0">
        <RelTime at={event.at} />
        <span className="min-w-[4ch] pt-px text-right text-xs [font-variant-numeric:tabular-nums_slashed-zero]">
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
