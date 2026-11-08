import { useQuery } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import { type ReactNode, useMemo } from "react";
import type {
  Agent,
  BoardEvent,
  ChildTask,
  Evidence,
  Feedback,
  Task,
  TaskDetail,
} from "@/api/schema.ts";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert.tsx";
import { Badge } from "@/components/ui/badge.tsx";
import { boardOfTask } from "@/lib/ids.ts";
import { cn } from "@/lib/utils.ts";
import { queries } from "@/live/query.ts";
import { Markdown, PlainText, SafeLink, safeHref } from "@/text/markdown.tsx";
import {
  AbsTime,
  agentIndex,
  Crumbs,
  Empty,
  Owner,
  PriorityBadge,
  QueryFrame,
  RelTime,
  SectionTitle,
  StatusBadge,
  useAgents,
  useTitle,
  Who,
} from "@/ui/common.tsx";
import { Icon, StatusIcon, statusKey } from "@/ui/icons.tsx";
import { StatusTally, taskTally } from "@/ui/tally.tsx";
import { EventLine, Timeline } from "./events.tsx";

const route = getRouteApi("/tasks/$id");

export function TaskScreen() {
  const { id } = route.useParams();
  const board = boardOfTask(id);
  const detail = useQuery(queries.task(id));
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);
  // Titles and statuses of blockers, subtasks and the parent (shared with the board screen).
  const boardTasks = useQuery(queries.boardTasks(board));
  const related = useMemo(
    () => new Map((boardTasks.data ?? []).map((task) => [task.id, task])),
    [boardTasks.data],
  );
  useTitle(detail.data ? `${detail.data.task.id} ${detail.data.task.title}` : id);
  // The crumb names the board as its page does, from the list the sidebar already holds.
  const slug = detail.data?.task.board ?? board;
  const boardTitle = useQuery(queries.boards()).data?.find((item) => item.slug === slug)?.title;

  return (
    <>
      <Crumbs current={id}>
        <Link to="/">Home</Link>
        <Icon name="chevron" className="size-3 opacity-70" />
        <Link to="/boards/$slug" params={{ slug }}>
          {boardTitle ?? slug}
        </Link>
      </Crumbs>
      <QueryFrame query={detail} label="Loading task">
        {(data) => <TaskView detail={data} agents={agents} related={related} />}
      </QueryFrame>
    </>
  );
}

/** A task section: heading, optional count, then its content. */
function Section({
  id,
  title,
  count,
  first,
  children,
}: {
  id: string;
  title: string;
  count?: number;
  first?: boolean;
  children: ReactNode;
}) {
  return (
    <section className={first ? undefined : "mt-7"} aria-labelledby={id}>
      <SectionTitle id={id} {...(count === undefined ? {} : { count })}>
        {title}
      </SectionTitle>
      {children}
    </section>
  );
}

function Muted({ children }: { children: ReactNode }) {
  return <p className="text-muted-foreground">{children}</p>;
}

/** Facts as a two-column list: label, then value. */
function Facts({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <dl
      className={cn(
        "m-0 grid [&_dd]:m-0 [&_dd]:flex [&_dd]:min-w-0 [&_dd]:flex-wrap [&_dd]:items-center [&_dd]:gap-x-2 [&_dd]:gap-y-1 [&_dt]:text-meta [&_dt]:text-muted-foreground [&>div]:grid [&>div]:items-baseline [&>div]:gap-2.5",
        className,
      )}
    >
      {children}
    </dl>
  );
}

function TaskView({
  detail,
  agents,
  related,
}: {
  detail: TaskDetail;
  agents: Map<string, Agent>;
  related: Map<string, Task>;
}) {
  const { task } = detail;
  const acceptance = task.acceptance
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  const summary = task.status === "done" ? task.summary : undefined;

  return (
    <article>
      <header className="mb-6">
        {task.parent && <ParentLine id={task.parent} related={related} />}
        <h1 className="max-w-[40ch] text-headline font-semibold tracking-[-0.015em] text-balance max-md:text-display-phone">
          <PlainText text={task.title} />
        </h1>
        {/* On narrow screens the properties follow the description; this line keeps the essentials on top. */}
        <p className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-2 text-meta lg:hidden">
          <StatusBadge status={task.status} ready={task.ready} />
          <PriorityBadge priority={task.priority} />
          <Owner task={task} agents={agents} />
        </p>
        {/* The links to other tasks are far down there too; this says they exist. */}
        {(task.blockers.length > 0 || detail.child_tasks.length > 0) && (
          <p className="mt-1.5 flex flex-wrap items-center gap-x-3 text-meta text-muted-foreground lg:hidden">
            {task.blockers.length > 0 && (
              <a
                className="inline-flex min-h-6 items-center gap-1 text-muted-foreground"
                href={`#${linksId("Blocked by")}`}
              >
                <Icon name="link" className="size-3.5" />
                Blocked by <span className="ident">{task.blockers.join(", ")}</span>
              </a>
            )}
            {detail.child_tasks.length > 0 && (
              <a
                className="inline-flex min-h-6 items-center text-muted-foreground"
                href="#task-subtasks"
              >
                {detail.child_tasks.length}{" "}
                {detail.child_tasks.length === 1 ? "subtask" : "subtasks"}
              </a>
            )}
          </p>
        )}
      </header>

      <div className="grid gap-7 lg:grid-cols-[minmax(0,1fr)_280px] lg:items-start lg:gap-8">
        <aside
          className="order-2 grid gap-5 max-lg:border-t max-lg:pt-5 lg:sticky lg:top-6 lg:border-l lg:pl-6"
          aria-label="Properties"
        >
          <Facts className="gap-3 text-meta [&>div]:grid-cols-[76px_minmax(0,1fr)] max-[520px]:[&>div]:grid-cols-[96px_minmax(0,1fr)]">
            <div>
              <dt>Status</dt>
              <dd>
                <StatusBadge status={task.status} ready={task.ready} />
                {task.status === "open" && !task.ready && task.blockers.length > 0 && (
                  <span className="text-muted-foreground">waiting on blockers</span>
                )}
              </dd>
            </div>
            <div>
              <dt>Priority</dt>
              <dd>
                <PriorityBadge priority={task.priority} />
              </dd>
            </div>
            <div>
              <dt>Owner</dt>
              <dd>
                <Owner task={task} agents={agents} withTitle />
              </dd>
            </div>
            <div>
              <dt>Autopilot</dt>
              <dd>
                {task.auto
                  ? "May start without you"
                  : task.proposal
                    ? "Proposal: waits for an agent's review"
                    : "Needs you to start"}
              </dd>
            </div>
            {task.not_before && (
              <div>
                <dt>Not before</dt>
                <dd>
                  <AbsTime at={task.not_before} />
                </dd>
              </div>
            )}
            <div>
              <dt>Created</dt>
              <dd>
                <RelTime at={task.created_at} /> by <Who id={task.created_by} agents={agents} />
              </dd>
            </div>
            <div>
              <dt>Updated</dt>
              <dd>
                <RelTime at={task.updated_at} />
              </dd>
            </div>
          </Facts>
          <TaskLinks title="Blocked by" ids={task.blockers} related={related} />
          <TaskLinks title="Blocks" ids={detail.blocks} related={related} />
        </aside>

        <div className="min-w-0">
          {task.blocked_reason && (
            <Alert variant="warn" className="mb-5 px-4.5 py-4">
              <AlertTitle className="text-sm font-semibold">Blocked</AlertTitle>
              <AlertDescription className="text-warn">
                <p>
                  <PlainText text={task.blocked_reason} />
                </p>
              </AlertDescription>
            </Alert>
          )}

          {task.feedback && (
            <FeedbackFacts feedback={task.feedback} agents={agents} related={related} />
          )}

          {summary && (
            <Section id="task-summary" title="Summary" first={!task.feedback}>
              <div className="max-w-[100ch] leading-relaxed">
                <Markdown text={summary} />
              </div>
            </Section>
          )}

          <Section id="task-description" title="Description" first={!summary && !task.feedback}>
            {task.body.trim() ? (
              <div className="max-w-[100ch] leading-relaxed">
                <Markdown text={task.body} />
              </div>
            ) : (
              <Muted>No description.</Muted>
            )}
          </Section>

          {detail.child_tasks.length > 0 && <Subtasks detail={detail} agents={agents} />}

          <Section id="task-acceptance" title="Acceptance" count={acceptance.length}>
            {acceptance.length > 0 ? (
              <ol className="divide-y overflow-hidden rounded-lg border">
                {acceptance.map((line, index) => (
                  <li key={index} className="flex gap-3 px-3.5 py-2.5">
                    <span
                      className="mt-px grid size-5 flex-none place-items-center rounded-full bg-secondary text-2xs font-semibold text-subtle-foreground tabular-nums"
                      aria-hidden="true"
                    >
                      {index + 1}
                    </span>
                    <PlainText text={line} />
                  </li>
                ))}
              </ol>
            ) : (
              <Muted>No acceptance checks written.</Muted>
            )}
          </Section>

          <Section id="task-evidence" title="Evidence" count={detail.evidence.length}>
            {detail.evidence.length > 0 ? (
              <ul className="divide-y overflow-hidden rounded-lg border bg-card shadow-raise">
                {detail.evidence.map((item, index) => (
                  <EvidenceItem key={index} item={item} agents={agents} />
                ))}
              </ul>
            ) : (
              <Muted>None yet. Done requires a check, commit, PR or artifact.</Muted>
            )}
          </Section>

          <Section id="task-notes" title="Notes" count={detail.notes.length}>
            {detail.notes.length > 0 ? (
              <ul className="grid gap-2.5">
                {detail.notes.map((note) => (
                  <li key={note.id} className="grid gap-1.5 rounded-lg border bg-card px-3.5 py-3">
                    <div className="flex flex-wrap items-center gap-2 text-meta">
                      <Who id={note.agent} agents={agents} />
                      <Badge
                        variant={
                          note.kind === "decision"
                            ? "review"
                            : note.kind === "finding"
                              ? "doing"
                              : "outline"
                        }
                        shape="tag"
                      >
                        {note.kind}
                      </Badge>
                      <span className="text-muted-foreground">
                        <RelTime at={note.at} />
                      </span>
                    </div>
                    <p className="max-w-[100ch]">
                      <PlainText text={note.text} />
                    </p>
                  </li>
                ))}
              </ul>
            ) : (
              <Muted>No notes.</Muted>
            )}
          </Section>

          <Section id="task-history" title="History">
            {detail.history.length > 0 ? (
              <Timeline>
                {[...detail.history]
                  .sort((a, b) => b.seq - a.seq)
                  .map((event: BoardEvent) => (
                    <EventLine key={event.seq} event={event} agents={agents} />
                  ))}
              </Timeline>
            ) : (
              <Empty title="No history" icon="events" />
            )}
          </Section>
        </div>
      </div>
    </article>
  );
}

function FeedbackFacts({
  feedback,
  agents,
  related,
}: {
  feedback: Feedback;
  agents: Map<string, Agent>;
  related: Map<string, Task>;
}) {
  const { resolution } = feedback;
  return (
    <Section id="task-feedback" title="Feedback" first>
      <Facts className="gap-2.5 rounded-lg border p-4 [&>div]:grid-cols-[100px_minmax(0,1fr)] max-[520px]:[&>div]:grid-cols-[96px_minmax(0,1fr)]">
        <div>
          <dt>About</dt>
          <dd>
            <code>{feedback.about}</code>
            {feedback.activity && (
              <span className="text-muted-foreground">while {feedback.activity}</span>
            )}
          </dd>
        </div>
        <div>
          <dt>Source</dt>
          <dd>{feedback.source}</dd>
        </div>
        <div>
          <dt>Reports</dt>
          <dd>
            <span>
              {feedback.reports} from{" "}
              {feedback.reporters.map((id, index) => (
                <span key={id}>
                  {index > 0 && ", "}
                  <Who id={id} agents={agents} />
                </span>
              ))}
            </span>
            <span className="text-muted-foreground">
              last <RelTime at={feedback.last_reported_at} />
            </span>
          </dd>
        </div>
        <div>
          <dt>Resolution</dt>
          <dd>
            {resolution ? (
              <>
                <Badge variant="outline" shape="tag">
                  {resolution.kind}
                </Badge>
                {resolution.task && <TaskRef id={resolution.task} related={related} />}
                {resolution.reason && <PlainText text={resolution.reason} />}
                <span className="text-muted-foreground">
                  by {resolution.by === "user" ? "you" : <Who id={resolution.by} agents={agents} />}{" "}
                  <RelTime at={resolution.at} />
                </span>
              </>
            ) : (
              <span className="text-muted-foreground">Unresolved</span>
            )}
          </dd>
        </div>
        {feedback.recurs && (
          <div>
            <dt>Recurs</dt>
            <dd>
              <TaskRef id={feedback.recurs} related={related} />
              <span className="text-muted-foreground">came back after that fix</span>
            </dd>
          </div>
        )}
      </Facts>
    </Section>
  );
}

/** A link to a task, with its status and title when it is on this board. */
function TaskRef({ id, related }: { id: string; related: Map<string, Task> }) {
  const task = related.get(id);
  return (
    <>
      <Link to="/tasks/$id" params={{ id }}>
        <span className="ident">{id}</span>
      </Link>
      {task && <StatusBadge status={task.status} ready={task.ready} />}
    </>
  );
}

/** The anchor of a list of linked tasks, such as "task-blocked-by". */
function linksId(title: string): string {
  return `task-${title.toLowerCase().replaceAll(" ", "-")}`;
}

function TaskLinks({
  title,
  ids,
  related,
}: {
  title: string;
  ids: string[];
  related: Map<string, Task>;
}) {
  if (ids.length === 0) return null;
  return (
    <section id={linksId(title)} className="scroll-mt-4 border-t pt-4">
      <h2 className="mb-2 text-meta font-medium text-muted-foreground">{title}</h2>
      <ul className="grid gap-1">
        {ids.map((id) => {
          const task = related.get(id);
          return (
            <li key={id}>
              <Link
                className="-mx-2 grid grid-cols-[14px_auto] items-center gap-x-2 gap-y-0.5 rounded-sm px-2 py-1.5 text-meta text-foreground hover:bg-secondary hover:no-underline"
                to="/tasks/$id"
                params={{ id }}
              >
                {task && <StatusIcon status={statusKey(task)} label />}
                <span className="ident col-start-2 text-xs text-muted-foreground">{id}</span>
                {task && <PlainText text={task.title} className="col-start-2 whitespace-normal" />}
              </Link>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function EvidenceItem({ item, agents }: { item: Evidence; agents: Map<string, Agent> }) {
  return (
    <li className="flex flex-wrap items-center gap-x-2.5 gap-y-1.5 px-3.5 py-2.5">
      <Badge variant="outline" shape="tag">
        {item.kind}
      </Badge>
      <span className="min-w-0 flex-[1_1_200px] [overflow-wrap:anywhere]">
        {safeHref(item.value) ? (
          <SafeLink href={item.value}>{item.value}</SafeLink>
        ) : (
          <code>{item.value}</code>
        )}
      </span>
      {item.exit_code !== null && (
        <Badge variant={item.exit_code === 0 ? "ok" : "bad"} shape="tag">
          exit {item.exit_code}
        </Badge>
      )}
      <span className="inline-flex items-center gap-1.5 text-meta text-muted-foreground">
        <Who id={item.agent} agents={agents} /> · <RelTime at={item.at} />
      </span>
    </li>
  );
}

/** Above the title: the umbrella task this one belongs to. */
function ParentLine({ id, related }: { id: string; related: Map<string, Task> }) {
  const parent = related.get(id);
  return (
    <p className="mb-2 flex min-w-0 flex-wrap items-center gap-x-1.5 text-meta text-muted-foreground">
      <Icon name="subtasks" className="size-3.5" />
      <span>Subtask of</span>
      <Link
        className="inline-flex min-h-6 min-w-0 items-center gap-1.5"
        to="/tasks/$id"
        params={{ id }}
      >
        {parent && <StatusIcon status={statusKey(parent)} label />}
        <span className="ident">{id}</span>
        {parent && <PlainText text={parent.title} className="whitespace-normal" />}
      </Link>
    </p>
  );
}

/**
 * A parent's children in one place: how they stand, then each with its status, title and
 * owner, by id number as the server lists them.
 */
function Subtasks({ detail, agents }: { detail: TaskDetail; agents: Map<string, Agent> }) {
  const { task, child_tasks: children } = detail;
  return (
    <Section id="task-subtasks" title="Subtasks" count={children.length}>
      <div className="grid gap-3 rounded-lg border bg-card px-3.5 pt-3 pb-1 shadow-raise">
        <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
          <StatusTally
            counts={taskTally(children)}
            label="Subtasks by status"
            className="min-w-0 flex-[1_1_240px]"
          />
          <Link
            className="inline-flex min-h-6 items-center gap-1 text-meta font-medium max-md:min-h-11"
            to="/boards/$slug"
            params={{ slug: task.board }}
            search={{ parent: task.id }}
          >
            Open as a list <Icon name="arrow" className="size-3.5" />
          </Link>
        </div>
        <ul className="-mx-3.5 divide-y border-t">
          {children.map((child) => (
            <ChildRow key={child.id} child={child} agents={agents} />
          ))}
        </ul>
      </div>
    </Section>
  );
}

function ChildRow({ child, agents }: { child: ChildTask; agents: Map<string, Agent> }) {
  return (
    <li
      className="grid grid-cols-[14px_auto_minmax(0,1fr)_auto] items-center gap-x-2.5 px-3.5 py-2 max-md:grid-cols-[14px_minmax(0,1fr)_auto] max-md:items-start max-md:gap-y-1"
      data-child={child.id}
    >
      <StatusIcon status={statusKey(child)} label className="max-md:mt-0.75" />
      <span className="ident text-xs text-subtle-foreground max-md:col-start-2 max-md:row-start-2">
        {child.id}
      </span>
      <Link
        className="min-w-0 font-medium text-foreground max-md:col-start-2 max-md:row-start-1"
        to="/tasks/$id"
        params={{ id: child.id }}
      >
        <PlainText text={child.title} className="whitespace-normal" />
      </Link>
      <span className="flex min-w-0 justify-end text-meta max-md:col-start-3 max-md:row-span-2 max-md:row-start-1 max-md:[&_[data-slot=avatar]]:hidden">
        <Owner task={child} agents={agents} />
      </span>
    </li>
  );
}
