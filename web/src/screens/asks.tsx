import { useForm } from "@tanstack/react-form";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import { useId, useMemo } from "react";
import { api } from "@/api/client.ts";
import type { Agent, Ask, Task } from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import { Label } from "@/components/ui/label.tsx";
import { Textarea } from "@/components/ui/textarea.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { useNow } from "@/live/runtime.ts";
import { Markdown } from "@/text/markdown.tsx";
import {
  agentIndex,
  AskKind,
  Crumbs,
  Empty,
  ErrorNotice,
  PageHeader,
  QueryFrame,
  RelTime,
  SectionTitle,
  useAgents,
  useTitle,
  Who,
} from "@/ui/common.tsx";
import { Icon } from "@/ui/icons.tsx";
import { TaskBoard, TaskGroupSection, TaskList } from "./board.tsx";
import { UpdatesSection } from "./updates.tsx";

const asksRoute = getRouteApi("/asks");
const askRoute = getRouteApi("/asks/$id");

const VIA: Record<string, string> = { web: "the web view", telegram: "Telegram", cli: "the CLI" };

/** The open asks, shared by the queue and the navigation badge. */
export function useOpenAsks() {
  return useQuery(queries.openAsks());
}

export function newestFirst(asks: Ask[]): Ask[] {
  return [...asks].sort(
    (a, b) => Date.parse(b.created_at) - Date.parse(a.created_at) || b.id - a.id,
  );
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
    const waits =
      task.status === "blocked" ||
      (task.status === "open" &&
        !task.auto &&
        !isProposal(task) &&
        task.feedback === null &&
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
      tasks: list.sort(
        (a, b) => a.priority.localeCompare(b.priority) || number(a.id) - number(b.id),
      ),
    }));
}

/** What the Needs you page lists across boards: the open asks and the waiting tasks. */
export function needsYouCount(
  openAsks: Ask[],
  tasks: Task[],
  now: number,
  closedBoards: ReadonlySet<string> = new Set(),
): number {
  return waitingTasks(tasks, openAsks, now, closedBoards).reduce(
    (sum, group) => sum + group.tasks.length,
    openAsks.length,
  );
}

/**
 * The navigation badge's count, from the page's own cached queries. Until the tasks load
 * it counts the open asks only.
 */
export function useNeedsYouCount(): number {
  const asks = useOpenAsks();
  const tasks = useQuery(queries.candidateTasks());
  const closed = useClosedBoards();
  const now = useNow(60_000);
  return needsYouCount(asks.data ?? [], tasks.data ?? [], now, closed);
}

/** Slugs of closed boards, from the boards query; none until it loads. */
function useClosedBoards(): ReadonlySet<string> {
  const boards = useQuery(queries.boards()).data;
  return useMemo(
    () =>
      new Set((boards ?? []).filter((item) => item.status === "closed").map((item) => item.slug)),
    [boards],
  );
}

export function AsksScreen() {
  useTitle("Needs you");
  const board = asksRoute.useSearch().board ?? "";
  const asks = useOpenAsks();
  const tasks = useQuery(queries.candidateTasks());
  const closed = useClosedBoards();
  const now = useNow(60_000);
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);

  return (
    <>
      <PageHeader
        title="Needs you"
        sub="Questions and requests from agents, newest first, then tasks waiting on you."
        aside={
          board && (
            <Badge variant="outline" shape="tag" className="h-7 text-meta" asChild>
              <Link to="/asks" aria-label={`Remove board filter ${board}`}>
                board: {board} <span aria-hidden="true">×</span>
              </Link>
            </Badge>
          )
        }
      />
      <QueryFrame query={asks} label="Loading asks">
        {(list) => {
          const shown = newestFirst(list.filter((ask) => !board || ask.board === board));
          return (
            <>
              {shown.length > 0 && (
                <div className="grid gap-4">
                  {shown.map((ask) => (
                    <AskCard key={ask.id} ask={ask} agents={agents} />
                  ))}
                </div>
              )}
              {/* Its own frame: asks stay answerable while tasks load or fail. */}
              <QueryFrame query={tasks} label="Loading tasks">
                {(taskList) => {
                  const waiting = waitingTasks(taskList, list, now, closed).filter(
                    (group) => !board || group.board === board,
                  );
                  if (waiting.length > 0) {
                    return (
                      <WaitingTasks
                        groups={waiting}
                        agents={agents}
                        now={now}
                        first={shown.length === 0}
                      />
                    );
                  }
                  if (shown.length > 0) return null;
                  return (
                    <Empty title="Nothing needs you" icon="inbox">
                      Agents' questions and requests appear here, and on Telegram if it is set up,
                      with tasks that wait on you.
                    </Empty>
                  );
                }}
              </QueryFrame>
            </>
          );
        }}
      </QueryFrame>
      <UpdatesSection board={board} />
    </>
  );
}

/** Tasks that wait on the user without an open ask, one group per board. */
function WaitingTasks({
  groups,
  agents,
  now,
  first,
}: {
  groups: BoardTasks[];
  agents: Map<string, Agent>;
  now: number;
  first: boolean;
}) {
  const count = groups.reduce((sum, group) => sum + group.tasks.length, 0);
  return (
    <section className={first ? undefined : "mt-9"} aria-labelledby="needs-you-tasks">
      <SectionTitle
        id="needs-you-tasks"
        count={count}
        icon={<Icon name="inbox" className="self-center text-amber" />}
      >
        Tasks waiting on you
      </SectionTitle>
      <p className="-mt-1.5 mb-3 max-w-[68ch] text-meta text-muted-foreground">
        Blocked tasks and tasks only you can start, with no open ask.
      </p>
      {/* Tasks only you can start keep their amber tag. */}
      <div className="[&_[data-tag=manual]]:border-amber/35 [&_[data-tag=manual]]:bg-amber-soft [&_[data-tag=manual]]:text-amber">
        <TaskBoard>
          {groups.map(({ board, tasks }) => (
            <TaskGroupSection
              key={board}
              id={`needs-you-board-${board}`}
              level={3}
              heading={
                <>
                  <Icon name="board" className="size-3.5 text-muted-foreground" />
                  <Link className="text-foreground" to="/boards/$slug" params={{ slug: board }}>
                    {board}
                  </Link>
                </>
              }
              count={tasks.length}
            >
              <TaskList tasks={tasks} agents={agents} now={now} />
            </TaskGroupSection>
          ))}
        </TaskBoard>
      </div>
    </section>
  );
}

/** `/asks/<id>`: the deep link sent with Telegram notifications. */
export function AskScreen() {
  const { id } = askRoute.useParams();
  const askId = Number(id);
  const valid = Number.isSafeInteger(askId) && askId > 0;
  useTitle(`Ask #${id}`);
  const ask = useQuery({ ...queries.ask(askId), enabled: valid });
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);

  return (
    <>
      <Crumbs current={`Ask #${id}`}>
        <Link to="/asks">Needs you</Link>
      </Crumbs>
      {valid ? (
        <QueryFrame query={ask} label="Loading ask">
          {(data) => (
            <div className="max-w-[720px]">
              <AskCard ask={data} agents={agents} standalone />
            </div>
          )}
        </QueryFrame>
      ) : (
        <Empty title={`“${id}” is not an ask number`}>Ask links look like /asks/12.</Empty>
      )}
    </>
  );
}

function AskCard({
  ask,
  agents,
  standalone,
}: {
  ask: Ask;
  agents: Map<string, Agent>;
  standalone?: boolean;
}) {
  const Heading = standalone ? "h1" : "h2";
  return (
    <article
      className={cn(
        "relative grid gap-3.5 overflow-hidden rounded-xl border bg-card px-5.5 pt-4.5 pb-5 shadow-raise",
        "before:absolute before:top-3.5 before:bottom-3.5 before:-left-px before:w-[3px] before:rounded-r-[3px]",
        ask.status !== "open"
          ? "before:bg-input"
          : ask.kind === "action"
            ? "before:bg-amber"
            : "before:bg-status-review",
      )}
      data-status={ask.status}
    >
      <header className="flex flex-wrap items-center gap-x-3.5 gap-y-1.5 text-meta">
        <AskKind kind={ask.kind} />
        <Heading className="text-meta font-semibold">
          {standalone ? (
            <>Ask #{ask.id}</>
          ) : (
            <Link className="text-foreground" to="/asks/$id" params={{ id: String(ask.id) }}>
              Ask #{ask.id}
            </Link>
          )}
        </Heading>
        <span className="inline-flex items-center gap-2">
          <Who id={ask.agent} agents={agents} />
          <span className="text-muted-foreground">
            <RelTime at={ask.created_at} />
          </span>
        </span>
        <span className="inline-flex items-center gap-1 sm:ml-auto">
          <Link to="/boards/$slug" params={{ slug: ask.board }}>
            {ask.board}
          </Link>
          {ask.task && (
            <>
              <Icon name="chevron" className="size-3 text-muted-foreground" />
              <Link to="/tasks/$id" params={{ id: ask.task }}>
                <span className="ident">{ask.task}</span>
              </Link>
            </>
          )}
        </span>
      </header>
      <div className="text-base leading-[1.55] text-foreground">
        <Markdown text={ask.question} />
      </div>
      {ask.status === "open" && <AnswerForm ask={ask} />}
      {ask.status === "answered" && (
        <div className="grid gap-1 rounded-sm bg-status-ready-soft px-3.5 py-3">
          <p className="text-meta font-semibold text-status-ready">
            Answered{ask.answered_via ? ` via ${VIA[ask.answered_via] ?? ask.answered_via}` : ""}
            {ask.answered_at && (
              <>
                {" "}
                · <RelTime at={ask.answered_at} />
              </>
            )}
          </p>
          <p className="whitespace-pre-wrap">{ask.answer}</p>
        </div>
      )}
      {ask.status === "withdrawn" && (
        <p className="text-muted-foreground">The agent withdrew this ask.</p>
      )}
    </article>
  );
}

/** The answer an ask's form sends: written text wins over a chosen option. */
export function answerOf(value: { choice: string; text: string }): string {
  return value.text.trim() || value.choice;
}

export function AnswerForm({ ask }: { ask: Ask }) {
  const id = useId();
  const client = useQueryClient();
  const mutation = useMutation({
    mutationFn: fetching((answer: string) => api.answerAsk(ask.id, answer)),
    onSuccess: () => invalidateTags(client, [tags.asks, tags.boards]),
  });
  const missingText =
    ask.options.length > 0 ? "Choose an answer or write your own first." : "Write an answer first.";
  const form = useForm({
    defaultValues: { choice: "", text: "" },
    validators: {
      // Set when Send is pressed with nothing chosen or written; any change clears it.
      onSubmit: ({ value }) => (answerOf(value) ? undefined : missingText),
    },
    listeners: {
      onChange: ({ formApi }) => {
        if (formApi.state.errorMap.onSubmit) formApi.setErrorMap({ onSubmit: undefined });
      },
    },
    onSubmit: ({ value }) => mutation.mutateAsync(answerOf(value)).catch(() => undefined),
    onSubmitInvalid: () => document.getElementById(`${id}-text`)?.focus(),
  });

  return (
    <form
      className="grid gap-3 border-t pt-3.5"
      noValidate
      onSubmit={(event) => {
        event.preventDefault();
        // The form's state, not this render's: a second Enter can come before the re-render.
        if (!form.state.isSubmitting) void form.handleSubmit();
      }}
    >
      <form.Subscribe
        selector={(state) => ({
          values: state.values,
          missing: state.errorMap.onSubmit,
        })}
      >
        {({ values, missing }) => {
          const answer = answerOf(values);
          return (
            <>
              {ask.options.length > 0 && (
                <fieldset className="m-0 grid gap-2 border-0 p-0">
                  <legend className="mb-2 p-0 text-meta font-medium text-subtle-foreground">
                    Choose an answer
                  </legend>
                  <div className="grid gap-2">
                    {ask.options.map((option, index) => (
                      <Label
                        key={index}
                        className="flex min-h-[42px] cursor-pointer items-center gap-2.5 rounded-sm border border-input bg-card px-3 py-2 text-sm leading-normal font-normal hover:bg-muted has-checked:border-primary has-checked:bg-primary-soft has-focus-visible:outline-2 has-focus-visible:outline-offset-2 has-focus-visible:outline-ring [&_input]:outline-none"
                      >
                        <input
                          type="radio"
                          name={`${id}-choice`}
                          className="m-0 size-4 flex-none cursor-pointer accent-primary"
                          value={option}
                          // Written text replaces the choice, so no option shows chosen then.
                          checked={!values.text.trim() && values.choice === option}
                          onChange={() => {
                            form.setFieldValue("choice", option);
                            form.setFieldValue("text", "");
                          }}
                        />
                        <span className="whitespace-pre-wrap">{option}</span>
                        {index === 0 && (
                          <Badge variant="accent" shape="tag">
                            Recommended
                          </Badge>
                        )}
                      </Label>
                    ))}
                  </div>
                </fieldset>
              )}
              <form.Field name="text">
                {(field) => (
                  <div className="grid gap-1.5">
                    <Label
                      htmlFor={`${id}-text`}
                      className="text-meta font-medium text-subtle-foreground"
                    >
                      {ask.options.length > 0 ? "Or write your own answer" : "Your answer"}
                    </Label>
                    <Textarea
                      id={`${id}-text`}
                      rows={1}
                      className="field-sizing-content max-h-60 min-h-9 resize-none bg-card py-1.75 text-sm dark:bg-card"
                      value={field.state.value}
                      aria-invalid={missing ? true : undefined}
                      aria-describedby={missing ? `${id}-missing` : undefined}
                      onChange={(event) => field.handleChange(event.currentTarget.value)}
                    />
                  </div>
                )}
              </form.Field>
              {missing && (
                <p id={`${id}-missing`} className="text-meta font-medium text-error" role="alert">
                  {missing}
                </p>
              )}
              {mutation.error && <ErrorNotice error={mutation.error} compact />}
              <div className="flex flex-wrap items-center gap-x-3.5 gap-y-2">
                <Button type="submit" disabled={mutation.isPending}>
                  {mutation.isPending ? "Sending…" : "Send answer"}
                </Button>
                {answer && !mutation.isPending && (
                  <span className="text-meta [overflow-wrap:anywhere] text-muted-foreground">
                    Sends “{answer}”
                  </span>
                )}
              </div>
            </>
          );
        }}
      </form.Subscribe>
    </form>
  );
}
