import { useCallback, useEffect, useState } from "preact/hooks";
import { ApiFailure, api } from "../api/client.ts";
import type { Agent, BoardEvent } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { useLiveEvents, useResource } from "../live/runtime.ts";
import {
  agentIndex,
  Empty,
  ErrorNotice,
  Loading,
  RelTime,
  useAgents,
  useTitle,
  Who,
} from "../ui/common.tsx";
import { Icon, type IconName } from "../ui/icons.tsx";

const PAGE = 100;
const NOISY = new Set(["lease_created", "lease_released", "lease_expired", "files_touched"]);

/** Events by sequence number, newest first. */
export function mergeEvents(current: BoardEvent[], incoming: BoardEvent[]): BoardEvent[] {
  const bySeq = new Map(current.map((event) => [event.seq, event]));
  for (const event of incoming) bySeq.set(event.seq, event);
  return [...bySeq.values()].sort((a, b) => b.seq - a.seq);
}

export function EventsScreen() {
  useTitle("Events");
  const agents = agentIndex(useAgents().data);
  const boards = useResource("boards", [tags.boards], api.boards);
  const [events, setEvents] = useState<BoardEvent[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState<ApiFailure | null>(null);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [board, setBoard] = useState("");
  const [showNoisy, setShowNoisy] = useState(false);

  const loadLatest = useCallback(async () => {
    setError(null);
    try {
      const { seq } = await api.health();
      const page = await api.events({ since: Math.max(0, seq - PAGE), limit: PAGE });
      setEvents((current) => mergeEvents(current, page.events));
      setLoaded(true);
    } catch (failure) {
      if (!(failure instanceof ApiFailure)) throw failure;
      setError(failure);
    }
  }, []);

  useEffect(() => void loadLatest(), [loadLatest]);
  useLiveEvents(useCallback((event: BoardEvent) => {
    setEvents((current) => mergeEvents(current, [event]));
  }, []));

  const oldest = events.at(-1)?.seq ?? 0;
  async function loadOlder() {
    setLoadingOlder(true);
    setError(null);
    try {
      const page = await api.events({ since: Math.max(0, oldest - 1 - PAGE), limit: PAGE });
      setEvents((current) => mergeEvents(current, page.events));
    } catch (failure) {
      if (!(failure instanceof ApiFailure)) throw failure;
      setError(failure);
    } finally {
      setLoadingOlder(false);
    }
  }

  const shown = events.filter((event) =>
    (!board || event.board === board) && (showNoisy || !NOISY.has(event.type))
  );

  return (
    <>
      <header class="page-head">
        <div class="page-title">
          <h1>Events</h1>
          <p class="page-sub">Everything agents and you did, live as it happens.</p>
        </div>
      </header>
      <form class="toolbar" onSubmit={(event) => event.preventDefault()}>
        <label class="select">
          <span class="select-label">Board</span>
          <select value={board} onChange={(event) => setBoard(event.currentTarget.value)}>
            <option value="">All boards</option>
            {(boards.data ?? []).map((item) => (
              <option key={item.slug} value={item.slug}>{item.title}</option>
            ))}
          </select>
        </label>
        <label class="toggle">
          <input
            type="checkbox"
            checked={showNoisy}
            onChange={(event) => setShowNoisy(event.currentTarget.checked)}
          />
          Show leases and file edits
        </label>
      </form>

      {error && <ErrorNotice error={error} retry={loaded ? loadOlder : loadLatest} compact={loaded} />}
      {!loaded && !error && <Loading label="Loading events" />}
      {loaded && shown.length === 0 && (
        <Empty title="No events yet" icon="events">New events appear here as they happen.</Empty>
      )}
      {shown.length > 0 && (
        <ol class="timeline event-feed" aria-live="polite" aria-relevant="additions">
          {shown.map((event) => (
            <EventLine key={event.seq} event={event} agents={agents} showTask />
          ))}
        </ol>
      )}
      {loaded && oldest > 1 && (
        <div class="more">
          <button type="button" class="button" disabled={loadingOlder} onClick={loadOlder}>
            {loadingOlder ? "Loading…" : "Load older events"}
          </button>
        </div>
      )}
    </>
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

/** Events that went wrong: blocked work and failed checks. Asks get their own kind colour. */
function isNotable(event: BoardEvent): boolean {
  const payload = (event.payload ?? {}) as Record<string, unknown>;
  if (event.type === "task_updated" && payload.to === "blocked") return true;
  return event.type === "evidence_added" && typeof payload.exit_code === "number" && payload.exit_code !== 0;
}

/** Routine bookkeeping that reads better quietly. */
const ROUTINE = new Set(["handoff_updated", "agent_state_changed", "lease_created", "lease_released", "lease_expired", "files_touched", "note_added"]);

/** Events about the agent itself, where the board adds nothing. */
const PERSONAL = new Set(["handoff_updated", "agent_state_changed"]);

export function EventLine(
  { event, agents, showTask }: { event: BoardEvent; agents: Map<string, Agent>; showTask: boolean },
) {
  const summary = detail(event);
  const [verb, suffix] = describe(event);
  const kind = event.type.split("_")[0] ?? "";
  const askKind = event.type === "ask_created" && (event.payload as { kind?: unknown } | null)?.kind === "action"
    ? "action"
    : "decision";
  const weight = event.type === "ask_created"
    ? `ask event-ask-${askKind}`
    : isNotable(event)
    ? "notable"
    : ROUTINE.has(event.type)
    ? "routine"
    : "normal";
  // Events that went wrong share one glyph, so failure reads apart from routine work.
  const icon: IconName = event.type === "ask_created"
    ? askKind === "action" ? "hand" : "question"
    : weight === "notable"
    ? "alert"
    : KIND_ICON[kind] ?? "events";
  return (
    <li class={`event event-${weight}`} data-kind={kind}>
      <span class="event-node" aria-hidden="true"><Icon name={icon} /></span>
      <div class="event-body">
        <p class="event-line">
          <Who id={event.actor} agents={agents} />
          <span class="event-type" title={event.type}>{verb}</span>
          {showTask && event.task && (
            <a href={`/tasks/${encodeURIComponent(event.task)}`}><span class="ident">{event.task}</span></a>
          )}
          {showTask && !event.task && event.board && !PERSONAL.has(event.type) && (
            <a href={`/boards/${encodeURIComponent(event.board)}`}><span class="ident">{event.board}</span></a>
          )}
          {suffix && <span class="event-type">{suffix}</span>}
        </p>
        {summary && <p class="event-payload">{summary}</p>}
      </div>
      <span class="event-when">
        <RelTime at={event.at} />
        <span class="event-seq">#{event.seq}</span>
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
  if (payload === null || typeof payload !== "object" || Array.isArray(payload)) return summarize(payload);
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
  return Object.entries(record).map(([key, value]) => `${key}: ${short(value)}`).join(" · ");
}

function short(value: unknown): string {
  const text = typeof value === "string" ? value : JSON.stringify(value) ?? "";
  return text.length > 120 ? `${text.slice(0, 119)}…` : text;
}
