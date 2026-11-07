// "Since you last looked" on the Needs you screen: tasks done and coordinators' turn endings
// since the user's seen cursor (`GET /v1/updates`). A coordinator's closing message can be
// followed at once by a wake-up that starts its next turn, so this is where the user reads it.

import type { ComponentChildren } from "preact";
import { useId, useState } from "preact/hooks";
import { type Api, ApiFailure, api, UPDATES_MAX } from "../api/client.ts";
import type { Agent, DoneUpdate, Said, Updates } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { cache, useResource } from "../live/runtime.ts";
import { Markdown, PlainText, SafeLink, safeHref } from "../text/markdown.tsx";
import {
  agentIndex,
  Empty,
  ErrorNotice,
  Query,
  RelTime,
  useAgents,
  Who,
} from "../ui/common.tsx";

/** Updates since the seen cursor, for one board or all; shared with the navigation dot. */
export function useUpdates(board = "") {
  return useResource(
    board ? `updates:${board}` : "updates",
    [tags.updates],
    () => api.updates(board ? { board } : {}),
  );
}

export function unseenCount(updates: Updates): number {
  return updates.done.length + updates.said.length;
}

/** The time of the newest update listed, as the server wrote it; null when none is. */
export function newestAt(updates: Updates): string | null {
  let newest: string | null = null;
  for (const at of [...updates.done.map((item) => item.done_at), ...updates.said.map((item) => item.at)]) {
    if (newest === null || Date.parse(at) > Date.parse(newest)) newest = at;
  }
  return newest;
}

/**
 * Mark everything shown as seen: the cursor moves to the newest shown update, not to now,
 * so updates that arrived after the list loaded stay unseen.
 */
export async function markShownSeen(client: Pick<Api, "markSeen">, updates: Updates): Promise<void> {
  const at = newestAt(updates);
  if (at !== null) await client.markSeen(at);
}

export function UpdatesSection({ board }: { board: string }) {
  const updates = useUpdates(board);
  const agents = agentIndex(useAgents().data);
  const titleId = useId();
  return (
    <section class="section updates" aria-labelledby={titleId}>
      <h2 id={titleId}>Since you last looked</h2>
      <Query resource={updates} label="Loading updates">
        {(data) => <UpdatesView updates={data} agents={agents} board={board} />}
      </Query>
    </section>
  );
}

/** The lists themselves; exported for tests. */
export function UpdatesView(
  { updates, agents, board, client = api }: {
    updates: Updates;
    agents: Map<string, Agent>;
    /** A board filter: the cursor is shared, so marking seen needs every board shown. */
    board: string;
    client?: Pick<Api, "markSeen">;
  },
) {
  const total = unseenCount(updates);
  return (
    <>
      <div class="updates-bar">
        <p class="muted updates-since">
          {updates.since ? <>Since <RelTime at={updates.since} /></> : "Everything kept so far"}
          {board && <> · {board} only</>}
        </p>
        {total > 0 && !board && <MarkSeen updates={updates} client={client} />}
      </div>
      {total > 0 && board && (
        <p class="muted updates-note">
          Seen is kept for all boards at once. <a href="/asks">Show every board</a> to mark updates seen.
        </p>
      )}
      {updates.more && (
        <p class="notice notice-warn compact">
          Only the newest {UPDATES_MAX} of each kind are listed; older ones were cut. Marking all
          seen covers them too. Each task's page and the event feed keep the full history.
        </p>
      )}
      {total === 0
        ? (
          <Empty title="You're up to date">
            Finished tasks and what coordinators said at the end of their turns appear here until you
            mark them seen.
          </Empty>
        )
        : (
          <>
            {updates.done.length > 0 && (
              <UpdateGroup title="Finished tasks" count={updates.done.length}>
                {updates.done.map((item) => <DoneCard key={item.task} item={item} agents={agents} />)}
              </UpdateGroup>
            )}
            {updates.said.length > 0 && (
              <UpdateGroup title="Coordinators said" count={updates.said.length}>
                {updates.said.map((item) => <SaidCard key={item.id} item={item} agents={agents} />)}
              </UpdateGroup>
            )}
          </>
        )}
    </>
  );
}

function MarkSeen({ updates, client }: { updates: Updates; client: Pick<Api, "markSeen"> }) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<ApiFailure | null>(null);

  async function mark() {
    if (pending) return;
    setPending(true);
    setError(null);
    try {
      await markShownSeen(client, updates);
      cache.invalidate([tags.updates]);
    } catch (failure) {
      if (!(failure instanceof ApiFailure)) throw failure;
      setError(failure);
    } finally {
      setPending(false);
    }
  }

  return (
    <div class="mark-seen">
      <button type="button" class="button" onClick={mark} disabled={pending}>
        {pending ? "Marking…" : "Mark all seen"}
      </button>
      {error && <ErrorNotice error={error} compact />}
    </div>
  );
}

function UpdateGroup({ title, count, children }: { title: string; count: number; children: ComponentChildren }) {
  return (
    <div class="update-group">
      <h3 class="group-title">
        {title} <span class="group-count">{count}</span>
      </h3>
      <div class="update-list">{children}</div>
    </div>
  );
}

/** PRs and commits: what the user opens to see the change. */
const SHOWN_EVIDENCE = new Set(["pr", "commit"]);

function DoneCard({ item, agents }: { item: DoneUpdate; agents: Map<string, Agent> }) {
  const evidence = item.evidence.filter((entry) => SHOWN_EVIDENCE.has(entry.kind));
  return (
    <article class="card update-card">
      <h4 class="update-title">
        <a href={`/tasks/${encodeURIComponent(item.task)}`}>
          <code>{item.task}</code> <PlainText text={item.title} />
        </a>
      </h4>
      <p class="update-meta">
        <a href={`/boards/${encodeURIComponent(item.board)}`}>{item.board}</a>
        {item.owner && <> · <Who id={item.owner} agents={agents} /></>}
        {" · done "}
        <RelTime at={item.done_at} />
        {item.parent && (
          <>
            {" · part of "}
            <a href={`/tasks/${encodeURIComponent(item.parent)}`}><code>{item.parent}</code></a>
          </>
        )}
      </p>
      {item.summary
        ? <div class="agent-text update-text"><Markdown text={item.summary} /></div>
        : <p class="muted update-text">No summary written.</p>}
      {evidence.length > 0 && (
        <ul class="update-evidence" aria-label="Evidence">
          {evidence.map((entry, index) => (
            <li key={index}>
              <span class="tag">{entry.kind === "pr" ? "PR" : "commit"}</span>
              <span class="evidence-value">
                {safeHref(entry.value)
                  ? <SafeLink href={entry.value}>{entry.value}</SafeLink>
                  : <code>{entry.value}</code>}
              </span>
            </li>
          ))}
        </ul>
      )}
    </article>
  );
}

function SaidCard({ item, agents }: { item: Said; agents: Map<string, Agent> }) {
  return (
    <article class="card update-card">
      <p class="update-meta">
        <Who id={item.agent} agents={agents} />
        {" on "}
        <a href={`/boards/${encodeURIComponent(item.board)}`}>{item.board}</a>
        {" · "}
        <RelTime at={item.at} />
      </p>
      <CollapsibleMarkdown text={item.text} />
    </article>
  );
}

/** Longer than this, or than `CLAMP_LINES` lines, and agent text starts collapsed. */
const CLAMP_CHARS = 360;
const CLAMP_LINES = 6;

export function isLong(text: string): boolean {
  return text.length > CLAMP_CHARS || text.split("\n").length > CLAMP_LINES;
}

/** Agent markdown, collapsed to a few lines when long, with a button to show all of it. */
export function CollapsibleMarkdown({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  if (!isLong(text)) return <div class="agent-text update-text"><Markdown text={text} /></div>;
  return (
    <div class="collapsible">
      <div id={id} class={open ? "agent-text update-text" : "agent-text update-text clamped"}>
        <Markdown text={text} />
      </div>
      <button
        type="button"
        class="link-button"
        aria-expanded={open}
        aria-controls={id}
        onClick={() => setOpen(!open)}
      >
        {open ? "Show less" : "Show all"}
      </button>
    </div>
  );
}
