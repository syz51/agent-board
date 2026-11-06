import { useLocation } from "preact-iso";
import { useId, useState } from "preact/hooks";
import { ApiFailure, api } from "../api/client.ts";
import type { Agent, Ask } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { cache, useResource } from "../live/runtime.ts";
import { Markdown } from "../text/markdown.tsx";
import {
  agentIndex,
  Empty,
  ErrorNotice,
  Query,
  RelTime,
  useAgents,
  useTitle,
  Who,
} from "../ui/common.tsx";

const VIA: Record<string, string> = { web: "the web view", telegram: "Telegram", cli: "the CLI" };

/** The open asks, shared by the queue and the navigation badge. */
export function useOpenAsks() {
  return useResource("asks:open", [tags.asks], () => api.asks({ status: "open" }));
}

export function newestFirst(asks: Ask[]): Ask[] {
  return [...asks].sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at) || b.id - a.id);
}

export function AsksScreen() {
  useTitle("Needs you");
  const location = useLocation();
  const board = location.query.board ?? "";
  const asks = useOpenAsks();
  const agents = agentIndex(useAgents().data);

  return (
    <>
      <header class="page-head">
        <h1>Needs you</h1>
        {board && (
          <a class="chip chip-remove" href="/asks" aria-label={`Remove board filter ${board}`}>
            board: {board} <span aria-hidden="true">×</span>
          </a>
        )}
      </header>
      <Query resource={asks} label="Loading asks">
        {(list) => {
          const shown = newestFirst(list.filter((ask) => !board || ask.board === board));
          if (shown.length === 0) {
            return (
              <Empty title="Nothing needs you">
                Agents' questions and requests appear here, and on Telegram if it is set up.
              </Empty>
            );
          }
          return (
            <div class="ask-list">
              {shown.map((ask) => <AskCard key={ask.id} ask={ask} agents={agents} />)}
            </div>
          );
        }}
      </Query>
    </>
  );
}

/** `/asks/<id>`: the deep link sent with Telegram notifications. */
export function AskScreen({ id }: { id: string }) {
  const askId = Number(id);
  const valid = Number.isSafeInteger(askId) && askId > 0;
  useTitle(`Ask #${id}`);
  const ask = useResource(valid ? `ask:${askId}` : null, [tags.asks], () => api.ask(askId));
  const agents = agentIndex(useAgents().data);

  return (
    <>
      <nav class="crumbs" aria-label="Breadcrumb">
        <a href="/asks">Needs you</a>
      </nav>
      {valid
        ? (
          <Query resource={ask} label="Loading ask">
            {(data) => <AskCard ask={data} agents={agents} standalone />}
          </Query>
        )
        : <Empty title={`“${id}” is not an ask number`}>Ask links look like /asks/12.</Empty>}
    </>
  );
}

function AskCard(
  { ask, agents, standalone }: { ask: Ask; agents: Map<string, Agent>; standalone?: boolean },
) {
  const Heading = standalone ? "h1" : "h2";
  return (
    <article class={`card ask-card ask-${ask.status}`}>
      <header class="ask-meta">
        <span class={ask.kind === "action" ? "badge tone-blocked" : "badge tone-review"}>
          {ask.kind === "action" ? "Action" : "Decision"}
        </span>
        <Heading class="ask-title">
          {standalone ? <>Ask #{ask.id}</> : <a href={`/asks/${ask.id}`}>Ask #{ask.id}</a>}
        </Heading>
        <span class="muted">
          <Who id={ask.agent} agents={agents} /> · <RelTime at={ask.created_at} />
        </span>
      </header>
      <p class="ask-context">
        <a href={`/boards/${encodeURIComponent(ask.board)}`}>{ask.board}</a>
        {ask.task && (
          <>
            {" · "}
            <a href={`/tasks/${encodeURIComponent(ask.task)}`}><code>{ask.task}</code></a>
          </>
        )}
      </p>
      <div class="ask-question agent-text">
        <Markdown text={ask.question} />
      </div>
      {ask.status === "open" && <AnswerForm ask={ask} />}
      {ask.status === "answered" && (
        <div class="answered">
          <p class="answered-head">
            Answered{ask.answered_via ? ` via ${VIA[ask.answered_via] ?? ask.answered_via}` : ""}
            {ask.answered_at && <> · <RelTime at={ask.answered_at} /></>}
          </p>
          <p class="answered-text">{ask.answer}</p>
        </div>
      )}
      {ask.status === "withdrawn" && <p class="muted">The agent withdrew this ask.</p>}
    </article>
  );
}

function AnswerForm({ ask }: { ask: Ask }) {
  const id = useId();
  const [choice, setChoice] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<ApiFailure | null>(null);
  const answer = text.trim() || choice || "";

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!answer || pending) return;
    setPending(true);
    setError(null);
    try {
      await api.answerAsk(ask.id, answer);
      cache.invalidate([tags.asks, tags.boards]);
    } catch (failure) {
      if (!(failure instanceof ApiFailure)) throw failure;
      setError(failure);
    } finally {
      setPending(false);
    }
  }

  return (
    <form class="answer" onSubmit={submit}>
      {ask.options.length > 0 && (
        <fieldset class="options">
          <legend>Choose an answer</legend>
          {ask.options.map((option, index) => (
            <label key={index} class="option">
              <input
                type="radio"
                name={`${id}-option`}
                value={option}
                checked={choice === option && !text.trim()}
                onChange={() => {
                  setChoice(option);
                  setText("");
                }}
              />
              <span class="option-text">{option}</span>
              {index === 0 && <span class="chip">recommended</span>}
            </label>
          ))}
        </fieldset>
      )}
      <label class="field">
        <span>{ask.options.length > 0 ? "Or write your own answer" : "Your answer"}</span>
        <textarea
          rows={3}
          value={text}
          onInput={(event) => setText(event.currentTarget.value)}
        />
      </label>
      {error && <ErrorNotice error={error} compact />}
      <div class="answer-actions">
        <button type="submit" class="button button-primary" disabled={!answer || pending}>
          {pending ? "Sending…" : "Send answer"}
        </button>
        {answer && !pending && <span class="muted answer-preview">Sends “{answer}”</span>}
      </div>
    </form>
  );
}
