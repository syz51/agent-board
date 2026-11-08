import { useForm } from "@tanstack/react-form";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import { useId, useMemo } from "react";
import { api } from "@/api/client.ts";
import type { Agent, Ask } from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import { Label } from "@/components/ui/label.tsx";
import { Textarea } from "@/components/ui/textarea.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { Markdown, PlainText } from "@/text/markdown.tsx";
import {
  agentIndex,
  AskKind,
  Crumbs,
  Empty,
  ErrorNotice,
  Owner,
  PageHeader,
  QueryFrame,
  RelTime,
  SidePanel,
  SidePanelSection,
  StatusBadge,
  useAgents,
  useTitle,
  Who,
} from "@/ui/common.tsx";
import { Icon } from "@/ui/icons.tsx";
import { HandoffView } from "./agents.tsx";

const asksRoute = getRouteApi("/asks");
const askRoute = getRouteApi("/asks/$id");

/** Whether the send shortcut is named ⌘ Enter, as on Apple keyboards, or Ctrl Enter. */
const MAC = typeof navigator !== "undefined" && /Mac|iP(hone|ad)/.test(navigator.platform);

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

export function AsksScreen() {
  useTitle("Needs you");
  const board = asksRoute.useSearch().board ?? "";
  const asks = useOpenAsks();
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);

  return (
    <>
      <PageHeader
        title="Needs you"
        sub="Questions and requests from agents that wait on your answer, newest first."
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
          if (shown.length === 0) {
            return (
              <Empty title="Nothing needs you" icon="inbox">
                {board
                  ? `No open asks on ${board}.`
                  : "When an agent needs your decision or action, its ask appears here, and on Telegram if it is set up."}
              </Empty>
            );
          }
          return (
            <div className="grid gap-3">
              {shown.map((ask) => (
                <AskCard key={ask.id} ask={ask} agents={agents} />
              ))}
            </div>
          );
        }}
      </QueryFrame>
    </>
  );
}

/** `/asks/<id>`: the deep link sent with Telegram notifications. */
export function AskScreen() {
  const { id } = askRoute.useParams();
  const askId = Number(id);
  const valid = Number.isSafeInteger(askId) && askId > 0;
  // The number as read, so `/asks/012` is titled "Ask #12".
  const label = valid ? `Ask #${askId}` : `Ask #${id}`;
  const ask = useQuery({ ...queries.ask(askId), enabled: valid });
  // Once loaded, the title says what kind of ask it is, as a card in a list does.
  const kind = ask.data && (ask.data.kind === "action" ? "Action" : "Decision");
  const title = kind ? `${kind} · ${label}` : label;
  useTitle(title);
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);

  return (
    <>
      <Crumbs current={label}>
        <Link to="/asks">Needs you</Link>
      </Crumbs>
      <PageHeader title={title} />
      {valid ? (
        <QueryFrame query={ask} label="Loading ask">
          {(data) => (
            // The ask fills the content width, its prose kept to a reading measure; what it is
            // about sits in a column beside it on wide screens and below it on narrow ones.
            <div className="grid items-start gap-x-8 gap-y-6 xl:grid-cols-[minmax(0,1fr)_300px]">
              <AskCard ask={data} agents={agents} standalone />
              <AskContext ask={data} agents={agents} />
            </div>
          )}
        </QueryFrame>
      ) : (
        <Empty title={`“${id}” is not an ask number`}>Ask links look like /asks/12.</Empty>
      )}
    </>
  );
}

/** Beside a single ask: the task it is about and the asking agent's handoff. */
function AskContext({ ask, agents }: { ask: Ask; agents: Map<string, Agent> }) {
  // The side panel, as on a task page: stacked beside the ask on wide screens, its sections
  // side by side below it on medium ones.
  return (
    <SidePanel
      className="sm:grid-cols-2 sm:divide-x sm:divide-y-0 xl:grid-cols-1 xl:divide-x-0 xl:divide-y"
      aria-label="About this ask"
    >
      {ask.task && (
        <SidePanelSection title="Task">
          <AskTask id={ask.task} agents={agents} />
        </SidePanelSection>
      )}
      <SidePanelSection title="Asked by">
        <Who id={ask.agent} agents={agents} />
        {/* As on the agents page, an ended agent's handoff is history, not context. */}
        {agents.get(ask.agent)?.state !== "ended" && <HandoffView agentId={ask.agent} stacked />}
      </SidePanelSection>
    </SidePanel>
  );
}

function AskTask({ id, agents }: { id: string; agents: Map<string, Agent> }) {
  const detail = useQuery(queries.task(id));
  if (detail.error && detail.data === undefined) {
    return <ErrorNotice error={detail.error} retry={() => void detail.refetch()} compact />;
  }
  if (detail.data === undefined) {
    return <p className="text-meta text-muted-foreground">Loading task…</p>;
  }
  const { task } = detail.data;
  return (
    <div className="grid gap-2">
      <Link
        className="line-clamp-2 text-sm font-medium text-balance [overflow-wrap:anywhere] text-foreground"
        to="/tasks/$id"
        params={{ id }}
        title={task.title}
      >
        <PlainText className="whitespace-normal" text={task.title} />
      </Link>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1.5 text-meta">
        <StatusBadge status={task.status} ready={task.ready} />
        <span className="ident text-muted-foreground">{id}</span>
        <Owner task={task} agents={agents} />
      </div>
    </div>
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
  return (
    <article
      className={cn(
        "relative grid gap-2.5 rounded-lg border border-l-3 bg-card pt-3 pr-4 pb-3.5 pl-3.5",
        ask.status !== "open"
          ? "border-l-input"
          : ask.kind === "action"
            ? "border-l-amber"
            : "border-l-status-review",
      )}
      data-status={ask.status}
    >
      <header className="flex flex-wrap items-center gap-x-3.5 gap-y-1.5 text-meta">
        {/* On its own page the ask's kind and number are the page title. */}
        {!standalone && <AskKind kind={ask.kind} />}
        {!standalone && (
          <h2 className="text-meta font-semibold">
            <Link className="text-foreground" to="/asks/$id" params={{ id: String(ask.id) }}>
              Ask #{ask.id}
            </Link>
          </h2>
        )}
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
      {/* The question runs the card's width, as the answer controls below it do. */}
      <div className="text-sm leading-normal text-foreground">
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
      className="grid gap-2.5 border-t pt-3"
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
                <fieldset className="m-0 grid gap-1.5 border-0 p-0">
                  <legend className="mb-1.5 p-0 text-meta font-medium text-subtle-foreground">
                    Choose an answer
                  </legend>
                  <div className="grid gap-1.5">
                    {ask.options.map((option, index) => (
                      <Label
                        key={index}
                        className="flex min-h-7.5 cursor-pointer items-center gap-2.5 rounded-md border border-input bg-card px-3 py-1 text-sm leading-normal font-normal hover:bg-muted has-checked:border-signal has-checked:bg-primary-soft has-focus-visible:outline-2 has-focus-visible:outline-offset-2 has-focus-visible:outline-ring [&_input]:outline-none"
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
                          // Neutral, so it reads apart from the violet of a decision.
                          <Badge variant="outline" shape="tag" className="gap-1">
                            <Icon name="check" className="size-3" />
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
                      rows={2}
                      // Two lines, so it reads as room to write; it grows with the text.
                      className="field-sizing-content max-h-60 min-h-15 resize-none bg-card py-1.75 text-sm dark:bg-card"
                      value={field.state.value}
                      aria-invalid={missing ? true : undefined}
                      aria-describedby={missing ? `${id}-missing` : undefined}
                      aria-keyshortcuts="Meta+Enter Control+Enter"
                      onChange={(event) => field.handleChange(event.currentTarget.value)}
                      onKeyDown={(event) => {
                        // Enter adds a line; with Cmd or Ctrl it sends, as in chat and mail.
                        if (event.key !== "Enter" || !(event.metaKey || event.ctrlKey)) return;
                        event.preventDefault();
                        event.currentTarget.form?.requestSubmit();
                      }}
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
              {/* Send closes the whole answer group, so it plainly sends a choice or the text.
                  It sits at the left, under the field, where the eye ends after typing. */}
              <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
                {/* The main action, quiet until there is an answer to send; a press with nothing
                    chosen says what is missing. */}
                <Button
                  type="submit"
                  variant={answer ? "default" : "outline"}
                  disabled={mutation.isPending}
                >
                  {mutation.isPending ? "Sending…" : "Send answer"}
                </Button>
                {answer && !mutation.isPending ? (
                  <span className="line-clamp-2 min-w-0 flex-1 text-meta [overflow-wrap:anywhere] text-muted-foreground">
                    Sends “{answer}”
                  </span>
                ) : (
                  <span
                    className="text-meta text-muted-foreground max-md:hidden"
                    aria-hidden="true"
                  >
                    or <kbd className="font-sans">{MAC ? "⌘" : "Ctrl"}&nbsp;Enter</kbd> in the field
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
