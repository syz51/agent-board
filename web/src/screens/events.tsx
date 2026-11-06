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
        <h1>Events</h1>
      </header>
      <form class="filters" onSubmit={(event) => event.preventDefault()}>
        <label class="field">
          <span>Board</span>
          <select value={board} onChange={(event) => setBoard(event.currentTarget.value)}>
            <option value="">All boards</option>
            {(boards.data ?? []).map((item) => (
              <option key={item.slug} value={item.slug}>{item.title}</option>
            ))}
          </select>
        </label>
        <label class="check">
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
        <Empty title="No events yet">New events appear here as they happen.</Empty>
      )}
      {shown.length > 0 && (
        <ol class="event-list event-feed" aria-live="polite" aria-relevant="additions">
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

export function EventLine(
  { event, agents, showTask }: { event: BoardEvent; agents: Map<string, Agent>; showTask: boolean },
) {
  const summary = summarize(event.payload);
  return (
    <li class="event">
      <div class="event-head">
        <span class="event-type">{event.type.replaceAll("_", " ")}</span>
        <Who id={event.actor} agents={agents} />
        {showTask && event.task && (
          <a href={`/tasks/${encodeURIComponent(event.task)}`}><code>{event.task}</code></a>
        )}
        {showTask && !event.task && event.board && (
          <a href={`/boards/${encodeURIComponent(event.board)}`}><code>{event.board}</code></a>
        )}
      </div>
      <span class="event-when">
        <RelTime at={event.at} /> <span class="muted">#{event.seq}</span>
      </span>
      {summary && <p class="event-payload">{summary}</p>}
    </li>
  );
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
