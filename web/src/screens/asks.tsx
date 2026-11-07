import { useLocation } from "preact-iso";
import { useId, useState } from "preact/hooks";
import { ApiFailure, api } from "../api/client.ts";
import type { Agent, Ask, Task } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { cache, useNow, useResource } from "../live/runtime.ts";
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
import { Icon } from "../ui/icons.tsx";
import { TaskRow } from "./board.tsx";

const VIA: Record<string, string> = { web: "the web view", telegram: "Telegram", cli: "the CLI" };

/** The open asks, shared by the queue and the navigation badge. */
export function useOpenAsks() {
  return useResource("asks:open", [tags.asks], () => api.asks({ status: "open" }));
}

export function newestFirst(asks: Ask[]): Ask[] {
  return [...asks].sort((a, b) => Date.parse(b.created_at) - Date.parse(a.created_at) || b.id - a.id);
}

/**
 * A pending proposal: open, not `auto`, with a body starting `Proposal:` after whitespace.
 * Agents review it, not the user (`is_proposal` in crates/board/src/daemon.rs).
 */
export function isProposal(task: Task): boolean {
  return task.status === "open" && !task.auto && task.body.trimStart().startsWith("Proposal:");
}

export interface BoardTasks {
  board: string;
  tasks: Task[];
}

/**
 * Tasks that wait on the user with no open ask, by board: blocked tasks, and open tasks
 * not marked `auto`. Left out: pending proposals and feedback items (agents review and
 * triage those), open tasks held by a `not_before` still ahead, and tasks on closed
 * boards. Exported for tests.
 */
export function waitingTasks(
  tasks: Task[],
  openAsks: Ask[],
  now: number,
  closedBoards: ReadonlySet<string> = new Set(),
): BoardTasks[] {
  const asked = new Set(openAsks.flatMap((ask) => (ask.task ? [ask.task] : [])));
  const byBoard = new Map<string, Task[]>();
  for (const task of tasks) {
    if (asked.has(task.id) || closedBoards.has(task.board)) continue;
    const waits = task.status === "blocked" ||
      (task.status === "open" && !task.auto && !isProposal(task) && task.feedback === null &&
        (task.not_before === null || Date.parse(task.not_before) <= now));
    if (!waits) continue;
    const list = byBoard.get(task.board) ?? [];
    list.push(task);
    byBoard.set(task.board, list);
  }
  const number = (id: string) => Number(id.slice(id.lastIndexOf("-") + 1));
  return [...byBoard]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([board, list]) => ({
      board,
      tasks: list.sort((a, b) => a.priority.localeCompare(b.priority) || number(a.id) - number(b.id)),
    }));
}

/** What the Needs you page lists across boards: the open asks and the waiting tasks. */
export function needsYouCount(
  openAsks: Ask[],
  tasks: Task[],
  now: number,
  closedBoards: ReadonlySet<string> = new Set(),
): number {
  return waitingTasks(tasks, openAsks, now, closedBoards)
    .reduce((sum, group) => sum + group.tasks.length, openAsks.length);
}

/**
 * The navigation badge's count, from the page's own cached queries. Until the tasks load
 * it counts the open asks only.
 */
export function useNeedsYouCount(): number {
  const asks = useOpenAsks();
  const tasks = useCandidateTasks();
  const closed = useClosedBoards();
  const now = useNow(60_000);
  return needsYouCount(asks.data ?? [], tasks.data ?? [], now, closed);
}

/** Slugs of closed boards, from the boards screen's cached query; none until it loads. */
function useClosedBoards(): ReadonlySet<string> {
  const boards = useResource("boards", [tags.boards], api.boards);
  return new Set((boards.data ?? []).filter((item) => item.status === "closed").map((item) => item.slug));
}

/** Open and blocked tasks on every board, shared by the page and the navigation badge. */
function useCandidateTasks() {
  return useResource("tasks:needs-you", [tags.tasks], async () => {
    const [open, blocked] = await Promise.all([
      api.tasks({ status: "open" }),
      api.tasks({ status: "blocked" }),
    ]);
    return [...open, ...blocked];
  });
}

export function AsksScreen() {
  useTitle("Needs you");
  const location = useLocation();
  const board = location.query.board ?? "";
  const asks = useOpenAsks();
  const tasks = useCandidateTasks();
  const closed = useClosedBoards();
  const now = useNow(60_000);
  const agents = agentIndex(useAgents().data);

  return (
    <>
      <header class="page-head">
        <div class="page-title">
          <h1>Needs you</h1>
          <p class="page-sub">Questions and requests from agents, newest first, then tasks waiting on you.</p>
        </div>
        {board && (
          <a class="tag tag-remove" href="/asks" aria-label={`Remove board filter ${board}`}>
            board: {board} <span aria-hidden="true">×</span>
          </a>
        )}
      </header>
      <Query resource={asks} label="Loading asks">
        {(list) => {
          const shown = newestFirst(list.filter((ask) => !board || ask.board === board));
          return (
            <>
              {shown.length > 0 && (
                <div class="ask-list">
                  {shown.map((ask) => <AskCard key={ask.id} ask={ask} agents={agents} />)}
                </div>
              )}
              {/* Its own frame: asks stay answerable while tasks load or fail. */}
              <Query resource={tasks} label="Loading tasks">
                {(taskList) => {
                  const waiting = waitingTasks(taskList, list, now, closed)
                    .filter((group) => !board || group.board === board);
                  if (waiting.length > 0) {
                    return <WaitingTasks groups={waiting} agents={agents} now={now} first={shown.length === 0} />;
                  }
                  if (shown.length > 0) return null;
                  return (
                    <Empty title="Nothing needs you" icon="inbox">
                      Agents' questions and requests appear here, and on Telegram if it is set up,
                      with tasks that wait on you.
                    </Empty>
                  );
                }}
              </Query>
            </>
          );
        }}
      </Query>
    </>
  );
}

/** Tasks that wait on the user without an open ask, one group per board. */
function WaitingTasks(
  { groups, agents, now, first }: { groups: BoardTasks[]; agents: Map<string, Agent>; now: number; first: boolean },
) {
  const count = groups.reduce((sum, group) => sum + group.tasks.length, 0);
  return (
    <section class={first ? "section section-first" : "section"} aria-labelledby="needs-you-tasks">
      <div class="section-head">
        <h2 id="needs-you-tasks">
          <Icon name="inbox" class="waiting-icon" />
          Tasks waiting on you
          <span class="section-count">{count}</span>
        </h2>
      </div>
      <p class="section-sub">Blocked tasks and tasks only you can start, with no open ask.</p>
      <div class="task-board waiting-board">
        {groups.map(({ board, tasks }) => (
          <section key={board} class="task-group" aria-labelledby={`needs-you-board-${board}`}>
            <h3 id={`needs-you-board-${board}`} class="group-head">
              <Icon name="board" />
              <a href={`/boards/${encodeURIComponent(board)}`}>{board}</a>
              <span class="group-count">{tasks.length}</span>
            </h3>
            <ul class="task-list">
              {tasks.map((task) => <TaskRow key={task.id} task={task} agents={agents} now={now} />)}
            </ul>
          </section>
        ))}
      </div>
    </section>
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
        <Icon name="chevron" />
        <span aria-current="page">Ask #{id}</span>
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
    <article class={`card ask-card ask-${ask.status} ask-kind-${ask.kind}`}>
      <header class="ask-meta">
        <span class={`kind kind-${ask.kind}`}>
          <Icon name={ask.kind === "action" ? "hand" : "question"} />
          {ask.kind === "action" ? "Action" : "Decision"}
        </span>
        <Heading class="ask-title">
          {standalone ? <>Ask #{ask.id}</> : <a href={`/asks/${ask.id}`}>Ask #{ask.id}</a>}
        </Heading>
        <span class="ask-from">
          <Who id={ask.agent} agents={agents} />
          <span class="muted">
            <RelTime at={ask.created_at} />
          </span>
        </span>
        <span class="ask-context">
          <a href={`/boards/${encodeURIComponent(ask.board)}`}>{ask.board}</a>
          {ask.task && (
            <>
              <Icon name="chevron" />
              <a href={`/tasks/${encodeURIComponent(ask.task)}`}><code>{ask.task}</code></a>
            </>
          )}
        </span>
      </header>
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
  // Set when Send is pressed with nothing chosen or written; cleared by any answer.
  const [missing, setMissing] = useState(false);
  const answer = text.trim() || choice || "";

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (pending) return;
    if (!answer) {
      setMissing(true);
      document.getElementById(`${id}-text`)?.focus();
      return;
    }
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
                  setMissing(false);
                }}
              />
              <span class="option-text">{option}</span>
              {index === 0 && <span class="tag tag-accent">Recommended</span>}
            </label>
          ))}
        </fieldset>
      )}
      <label class="field">
        <span>{ask.options.length > 0 ? "Or write your own answer" : "Your answer"}</span>
        <textarea
          id={`${id}-text`}
          rows={2}
          value={text}
          aria-invalid={missing || undefined}
          aria-describedby={missing ? `${id}-missing` : undefined}
          onInput={(event) => {
            setText(event.currentTarget.value);
            setMissing(false);
          }}
        />
      </label>
      {missing && (
        <p id={`${id}-missing`} class="field-error" role="alert">
          {ask.options.length > 0 ? "Choose an answer or write your own first." : "Write an answer first."}
        </p>
      )}
      {error && <ErrorNotice error={error} compact />}
      <div class="answer-actions">
        <button type="submit" class="button button-primary" disabled={pending}>
          {pending ? "Sending…" : "Send answer"}
        </button>
        {answer && !pending && <span class="muted answer-preview">Sends “{answer}”</span>}
      </div>
    </form>
  );
}
