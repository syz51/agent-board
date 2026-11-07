import { useState } from "preact/hooks";
import { ApiFailure, api } from "../api/client.ts";
import type { Agent, Board } from "../api/schema.ts";
import { PlainText } from "../text/markdown.tsx";
import { tags } from "../live/refresh.ts";
import { cache, useResource } from "../live/runtime.ts";
import {
  AgentStateBadge,
  agentIndex,
  Empty,
  ErrorNotice,
  Query,
  RelTime,
  useAgents,
  useTitle,
  Who,
} from "../ui/common.tsx";
import { Icon, StatusIcon } from "../ui/icons.tsx";
import { newestFirst, useOpenAsks } from "./asks.tsx";
import { ConfirmDialog } from "../ui/confirm.tsx";

export function BoardsScreen() {
  useTitle("Boards");
  const boards = useResource("boards", [tags.boards], api.boards);
  const agents = agentIndex(useAgents().data);
  const asks = useOpenAsks();
  const openAsks = asks.data ?? [];

  return (
    <>
      <header class="page-head">
        <div class="page-title">
          <h1>Boards</h1>
          {boards.data && (
            <p class="page-sub">
              {boards.data.length} board{boards.data.length === 1 ? "" : "s"}
              {openAsks.length > 0 && <> · {openAsks.length} waiting on you</>}
            </p>
          )}
        </div>
      </header>
      {openAsks.length > 0 && (
        <section class="waiting" aria-labelledby="waiting-title">
          <div class="section-head">
            <h2 id="waiting-title">Waiting on you</h2>
            <a class="section-link" href="/asks">
              All asks <Icon name="arrow" />
            </a>
          </div>
          <ul class="list ask-preview">
            {newestFirst(openAsks).slice(0, 4).map((ask) => (
              <li key={ask.id}>
                <a class="row-link" href={`/asks/${ask.id}`}>
                  <span class="ask-preview-side">
                    <span class={`kind kind-${ask.kind}`}>
                      <Icon name={ask.kind === "action" ? "hand" : "question"} />
                      {ask.kind === "action" ? "Action" : "Decision"}
                    </span>
                    <span class="ask-preview-meta">
                      {ask.board} · <RelTime at={ask.created_at} />
                    </span>
                  </span>
                  <span class="ask-preview-text" title={plainLine(ask.question)}>{plainLine(ask.question)}</span>
                </a>
              </li>
            ))}
          </ul>
        </section>
      )}
      <Query resource={boards} label="Loading boards">
        {(list) =>
          list.length === 0
            ? (
              <Empty title="No boards yet">
                Create one with <code>board create</code>; it appears here.
              </Empty>
            )
            : (
              <div class="board-grid">
                {list.map((board) => (
                  <BoardCard
                    key={board.slug}
                    board={board}
                    coordinator={board.coordinator ? agents.get(board.coordinator) : undefined}
                    agents={agents}
                  />
                ))}
              </div>
            )}
      </Query>
    </>
  );
}

/** The first line of agent markdown without its markup, for a one-line preview. */
export function plainLine(text: string): string {
  const line = text.trim().split("\n")[0] ?? "";
  return line
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/(\*\*|__|`|~~)/g, "")
    .replace(/^#+\s*/, "");
}

const SEGMENTS = [
  { key: "done", label: "done" },
  { key: "review", label: "review" },
  { key: "doing", label: "doing" },
  { key: "blocked", label: "blocked" },
  { key: "ready", label: "ready" },
  { key: "waiting", label: "waiting" },
] as const;

function segmentCounts(counts: Board["counts"]): Record<(typeof SEGMENTS)[number]["key"], number> {
  return {
    done: counts.done,
    review: counts.review,
    doing: counts.doing,
    blocked: counts.blocked,
    ready: counts.ready,
    waiting: Math.max(0, counts.open - counts.ready),
  };
}

function BoardCard(
  { board, coordinator, agents }: { board: Board; coordinator: Agent | undefined; agents: Map<string, Agent> },
) {
  const counts = segmentCounts(board.counts);
  const total = SEGMENTS.reduce((sum, segment) => sum + counts[segment.key], 0);
  return (
    <article class="card board-card">
      <header class="board-card-head">
        <h2 class="card-title">
          <a href={`/boards/${encodeURIComponent(board.slug)}`}>{board.title}</a>
        </h2>
        <span class="slug ident">{board.slug}</span>
        <span class="board-tags">
          {board.status !== "active" && (
            <span class={`pill board-${board.status}`}>
              <span class="state-dot" aria-hidden="true" />
              {board.status[0]?.toUpperCase()}{board.status.slice(1)}
            </span>
          )}
          {board.temporary && <span class="tag">Temporary</span>}
        </span>
      </header>
      <p class={board.goal ? "board-goal" : "board-goal muted"}>
        {board.goal ? <PlainText text={board.goal} /> : "No goal written."}
      </p>

      <div class="progress" aria-hidden="true">
        {SEGMENTS.map((segment) =>
          counts[segment.key] > 0 && (
            <span
              key={segment.key}
              class={`progress-seg seg-${segment.key}`}
              style={{ flexGrow: counts[segment.key] }}
            />
          )
        )}
      </div>
      {total === 0
        ? <p class="tally muted">No tasks yet</p>
        : (
          <ul class="tally" aria-label="Tasks by status">
            {SEGMENTS.map((segment) =>
              counts[segment.key] > 0 && (
                <li key={segment.key}>
                  <StatusIcon status={segment.key === "waiting" ? "open" : segment.key} />
                  <strong>{counts[segment.key]}</strong> {segment.label}
                </li>
              )
            )}
          </ul>
        )}

      <footer class="board-foot">
        <div class="coordinator">
          {board.coordinator === null
            ? <span class="muted">{board.autopilot ? "No coordinator running" : "No coordinator"}</span>
            : coordinator
            ? (
              <>
                <Who id={coordinator.id} agents={agents} />
                <AgentStateBadge agent={coordinator} />
              </>
            )
            : <Who id={board.coordinator} agents={agents} />}
        </div>
        {board.open_asks > 0 && (
          <a class="asks-link has-asks" href={`/asks?board=${encodeURIComponent(board.slug)}`}>
            <Icon name="inbox" />
            {board.open_asks} {board.open_asks === 1 ? "ask" : "asks"}
          </a>
        )}
        <AutopilotSwitch board={board} />
      </footer>
    </article>
  );
}

function AutopilotSwitch({ board }: { board: Board }) {
  const [confirming, setConfirming] = useState(false);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<ApiFailure | null>(null);
  const turningOn = !board.autopilot;

  async function apply() {
    setConfirming(false);
    setPending(true);
    setError(null);
    try {
      await api.setAutopilot(board.slug, turningOn);
      cache.invalidate([tags.boards, tags.board(board.slug)]);
    } catch (failure) {
      if (!(failure instanceof ApiFailure)) throw failure;
      setError(failure);
    } finally {
      setPending(false);
    }
  }

  return (
    <div class="autopilot">
      <button
        type="button"
        role="switch"
        class="switch"
        aria-checked={board.autopilot}
        aria-busy={pending}
        disabled={pending}
        onClick={() => setConfirming(true)}
      >
        <span class="switch-track" aria-hidden="true"><span class="switch-thumb" /></span>
        <span class="switch-label">
          Autopilot<span class="sr-only"> {pending ? "…" : board.autopilot ? "on" : "off"}</span>
        </span>
      </button>
      <ConfirmDialog
        open={confirming}
        title={`Turn autopilot ${turningOn ? "on" : "off"} for ${board.title}?`}
        confirmLabel={turningOn ? "Turn on" : "Turn off"}
        onConfirm={apply}
        onCancel={() => setConfirming(false)}
      >
        {turningOn
          ? (
            <p>
              A coordinator thread starts on one of the board's home devices and works through
              ready tasks marked auto, one at a time, without waiting for you. It sends an ask
              when it needs a decision.
            </p>
          )
          : (
            <p>
              No coordinator will pick up ready tasks; they wait for an agent or for you to start
              them.
            </p>
          )}
      </ConfirmDialog>
      {error && <ErrorNotice error={error} compact />}
    </div>
  );
}
