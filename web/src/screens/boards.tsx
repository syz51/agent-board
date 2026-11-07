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
  useAgents,
  useTitle,
} from "../ui/common.tsx";
import { ConfirmDialog } from "../ui/confirm.tsx";

export function BoardsScreen() {
  useTitle("Boards");
  const boards = useResource("boards", [tags.boards], api.boards);
  const agents = agentIndex(useAgents().data);

  return (
    <>
      <header class="page-head">
        <h1>Boards</h1>
      </header>
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
                  />
                ))}
              </div>
            )}
      </Query>
    </>
  );
}

const COUNT_ORDER = ["ready", "open", "doing", "blocked", "review", "done"] as const;

function BoardCard({ board, coordinator }: { board: Board; coordinator: Agent | undefined }) {
  return (
    <article class="card board-card">
      <div class="board-intro">
        <header class="card-head">
          <h2 class="card-title">
            <a href={`/boards/${encodeURIComponent(board.slug)}`}>{board.title}</a>
          </h2>
          <code class="muted">{board.slug}</code>
          {board.status !== "active" && <span class="badge tone-dropped">{board.status[0]?.toUpperCase()}{board.status.slice(1)}</span>}
          {board.temporary && <span class="badge tone-open">Temporary</span>}
        </header>
        {board.goal && <p class="board-goal"><PlainText text={board.goal} /></p>}
      </div>

      <dl class="counts">
        {COUNT_ORDER.map((key) => (
          <div key={key} class={`count count-${key}${board.counts[key] === 0 ? " is-zero" : ""}`}>
            <dt>{key}</dt>
            <dd>{board.counts[key]}</dd>
          </div>
        ))}
      </dl>

      <div class="board-foot">
        <a
          class={board.open_asks > 0 ? "asks-link has-asks" : "asks-link"}
          href={`/asks?board=${encodeURIComponent(board.slug)}`}
        >
          {board.open_asks === 0
            ? "No open asks"
            : `${board.open_asks} open ask${board.open_asks === 1 ? "" : "s"}`}
        </a>
        <AutopilotSwitch board={board} />
      </div>

      <div class="coordinator">
        <span class="label">Coordinator</span>
        {board.coordinator === null
          ? <span class="muted">{board.autopilot ? "None running" : "None"}</span>
          : coordinator
          ? (
            <>
              <span class="handle" title={coordinator.id}>{coordinator.handle}</span>
              <AgentStateBadge agent={coordinator} />
              <span class="muted">on {coordinator.device}</span>
            </>
          )
          : <span class="handle">{board.coordinator}</span>}
      </div>
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
          Autopilot <strong>{pending ? "…" : board.autopilot ? "on" : "off"}</strong>
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
