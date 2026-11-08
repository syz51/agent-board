import { useQuery } from "@tanstack/react-query";
import { getRouteApi, Link, useLocation } from "@tanstack/react-router";
import type { Agent, Task } from "@/api/schema.ts";
import { cn } from "@/lib/utils.ts";
import { queries } from "@/live/query.ts";
import { useNow } from "@/live/runtime.ts";
import { PlainText } from "@/text/markdown.tsx";
import {
  agentAnchor,
  AgentStateBadge,
  Empty,
  ErrorNotice,
  PageHeader,
  QueryFrame,
  RelTime,
  threadLink,
  useAgents,
  useTitle,
} from "@/ui/common.tsx";
import { Avatar, Icon, StatusIcon } from "@/ui/icons.tsx";
import { CollapsibleMarkdown } from "@/text/collapsible.tsx";

const route = getRouteApi("/agents");

const STATE_ORDER: Record<Agent["state"], number> = { running: 0, paused: 1, idle: 2, ended: 3 };
const FILTERS = [
  { value: "", label: "Active" },
  { value: "running", label: "Running" },
  { value: "paused", label: "Paused" },
  { value: "idle", label: "Idle" },
  { value: "away", label: "Away" },
  { value: "ended", label: "Ended" },
  { value: "all", label: "All" },
] as const;

/**
 * How long an agent that has not ended may go unseen before it counts as away: the server's
 * default `doing_idle_secs`. A session that crashed or was killed never sends `ended`, and a
 * live one is seen at every tool call and turn.
 */
const AWAY_AFTER_MS = 30 * 60_000;

/**
 * Whether an agent that has not ended is away: unseen for {@link AWAY_AFTER_MS}, and not
 * waiting. Waiting are agents paused for a usage-limit reset still ahead or, with no reset,
 * for the user, and a board's coordinator, whose lease the daemon renews without marking it
 * seen. The server's board listing keeps the same agents. Exported for tests.
 */
export function isAway(agent: Agent, now: number, coordinators: ReadonlySet<string>): boolean {
  if (agent.state === "ended" || now - Date.parse(agent.last_seen) <= AWAY_AFTER_MS) return false;
  if (agent.paused_until ? Date.parse(agent.paused_until) > now : agent.state === "paused") {
    return false;
  }
  return !coordinators.has(agent.id);
}

/**
 * Agents for a filter, busiest first. Active and each state leave out away agents, which
 * `away` lists; `ended` and `all` ignore it. Exported for tests.
 */
export function sortAgents(
  agents: Agent[],
  filter: string,
  now: number,
  coordinators: ReadonlySet<string>,
): Agent[] {
  return agents
    .filter((agent) => {
      if (filter === "all") return true;
      if (filter === "ended") return agent.state === "ended";
      const away = isAway(agent, now, coordinators);
      if (filter === "away") return away;
      return !away && (filter ? agent.state === filter : agent.state !== "ended");
    })
    .sort(
      (a, b) =>
        STATE_ORDER[a.state] - STATE_ORDER[b.state] ||
        Date.parse(b.last_seen) - Date.parse(a.last_seen),
    );
}

export function AgentsScreen() {
  useTitle("Agents");
  const state = route.useSearch().state ?? "";
  // The card another page linked to. CSS `:target` misses the router's in-app navigation.
  const target = useLocation({ select: (location) => location.hash });
  const agents = useAgents();
  const doing = useQuery(queries.doingTasks());
  const boards = useQuery(queries.boards());
  const now = useNow();
  const current = new Map<string, Task>();
  for (const task of doing.data ?? []) if (task.owner) current.set(task.owner, task);
  const coordinators = new Set<string>();
  for (const board of boards.data ?? []) if (board.coordinator) coordinators.add(board.coordinator);
  const count = (filter: string) =>
    agents.data ? sortAgents(agents.data, filter, now, coordinators).length : 0;

  return (
    <>
      <PageHeader
        title="Agents"
        sub={
          agents.data && (
            <>
              {count("running")} running · {count("paused")} paused · {count("idle")} idle ·{" "}
              {count("away")} away
            </>
          )
        }
      />
      <nav
        className="-mt-2 mb-5 inline-flex max-w-full gap-0.5 overflow-x-auto rounded-lg bg-secondary p-0.75 [scrollbar-width:none] max-md:flex"
        aria-label="Filter agents by state"
      >
        {FILTERS.map((filter) => (
          <Link
            key={filter.value}
            to="/agents"
            search={filter.value ? { state: filter.value } : {}}
            activeOptions={{ exact: true }}
            className="inline-flex min-h-7 flex-none items-center rounded-md px-3 py-1 text-meta font-medium text-subtle-foreground hover:text-foreground hover:no-underline aria-[current=page]:bg-card aria-[current=page]:font-semibold aria-[current=page]:text-foreground aria-[current=page]:shadow-[0_1px_2px_rgb(16_24_40/0.1),0_0_0_1px_var(--border)] max-md:flex-[1_0_auto] max-md:justify-center max-md:px-2"
          >
            {filter.label}
          </Link>
        ))}
      </nav>
      {doing.error && (
        <ErrorNotice error={doing.error} retry={() => void doing.refetch()} compact />
      )}
      <QueryFrame query={agents} label="Loading agents">
        {(list) => {
          const shown = sortAgents(list, state, now, coordinators);
          if (shown.length === 0) {
            return (
              <Empty
                title={list.length === 0 ? "No agents have registered" : "No agents in this state"}
              >
                {list.length === 0 && "Agents register when a session with the board hooks starts."}
              </Empty>
            );
          }
          return (
            <div className="edge @container divide-y border-y">
              <div
                className={`${ROW} hidden h-8 items-center bg-muted text-xs font-medium text-muted-foreground @min-[820px]:grid`}
                aria-hidden="true"
              >
                <span>Agent</span>
                <span>Working on</span>
              </div>
              {shown.map((agent) => (
                <AgentCard
                  key={agent.id}
                  agent={agent}
                  task={current.get(agent.id)}
                  targeted={target === agentAnchor(agent.handle)}
                />
              ))}
            </div>
          );
        }}
      </QueryFrame>
    </>
  );
}

/**
 * The agents list's columns from 820px of width: who and where it runs, then what it is
 * working on, which takes the rest. Two columns leave no empty band between them.
 */
const ROW = "grid gap-x-6 px-(--gutter) @min-[820px]:grid-cols-[380px_minmax(0,1fr)]";

/**
 * One agent per row: who, its state and where it runs, then its task, handoff and, for a
 * coordinator, its turn endings. Narrow lists stack the two.
 */
function AgentCard({
  agent,
  task,
  targeted,
}: {
  agent: Agent;
  task: Task | undefined;
  targeted: boolean;
}) {
  const where = [
    agent.role === "coordinator" ? "Coordinator" : null,
    agent.device,
    agent.runtime,
    agent.repo?.replace(/^github\.com\//, ""),
    agent.branch,
  ]
    .filter(Boolean)
    .join(" · ");
  const thread = threadLink(agent);
  return (
    <article
      id={agentAnchor(agent.handle)}
      className={cn(
        `${ROW} scroll-mt-4 gap-y-2.5 py-3 hover:bg-muted/60`,
        targeted && "bg-accent hover:bg-accent",
      )}
    >
      <header className="grid grid-cols-[28px_minmax(0,1fr)] content-start items-center justify-items-start gap-x-2.5 gap-y-1">
        <Avatar handle={agent.handle} large className="row-span-3 size-7 self-start text-2xs" />
        {/* The thread's title names the agent where it has one; the handle, which the rest
            of the view shows, stays beside it. */}
        <h2 className="flex max-w-full min-w-0 flex-wrap items-baseline gap-x-2 text-sm font-semibold">
          {agent.title && (
            <span className="[overflow-wrap:anywhere]">
              <PlainText className="whitespace-normal" text={agent.title} />
            </span>
          )}
          <span
            className={cn(
              "[font-variant-numeric:tabular-nums_slashed-zero]",
              agent.title && "ident text-meta font-medium text-muted-foreground",
            )}
            title={agent.id}
          >
            {agent.handle}
          </span>
        </h2>
        {/* Its state and when it was last seen share a line: together they answer "is it
            alive?". The thread link sits at its end. */}
        <div className="flex flex-wrap items-center gap-x-2.5 gap-y-0.5">
          <AgentStateBadge agent={agent} />
          <span className="text-meta text-muted-foreground">
            seen <RelTime at={agent.last_seen} />
          </span>
          {thread && (
            <a
              className="inline-flex min-h-6 items-center gap-1 text-meta font-medium max-md:min-h-11"
              href={thread}
              target="_blank"
              rel="noopener noreferrer"
            >
              Open in T3 Code <Icon name="external" className="size-3.5" />
            </a>
          )}
        </div>
        {/* Where it runs, as one quiet line under who it is; a line too long for the column
            ends in an ellipsis and shows whole on hover. */}
        <p
          className="col-start-2 w-full truncate text-meta text-muted-foreground"
          data-slot="where"
          title={where}
        >
          {where}
        </p>
      </header>
      <div className="grid min-w-0 content-start gap-2">
        <p className="font-medium">
          {task ? (
            // One run of text, so the id and the title share a line and the title wraps after
            // it, never onto a line of its own.
            <Link
              className="block min-h-6 py-0.5 text-foreground"
              to="/tasks/$id"
              params={{ id: task.id }}
            >
              <StatusIcon status="doing" className="mr-2 inline-block align-[-2px]" />
              <span className="ident mr-2 text-meta text-muted-foreground">{task.id}</span>
              <PlainText className="whitespace-normal" text={task.title} />
            </Link>
          ) : (
            <span className="text-muted-foreground">No task in doing</span>
          )}
        </p>
        {agent.state !== "ended" && <HandoffView agentId={agent.id} />}
        {agent.role === "coordinator" && <SaidView agentId={agent.id} />}
      </div>
    </article>
  );
}

/**
 * An agent's handoff, its labels left of their values. Beside a task, a rule sets it off;
 * `stacked`, for a narrow card, drops the rule and narrows the label column.
 */
export function HandoffView({ agentId, stacked }: { agentId: string; stacked?: boolean }) {
  const handoff = useQuery(queries.handoff(agentId));
  const handoffBox = stacked ? "grid gap-1 text-meta" : "grid gap-1 border-l-2 pl-3 text-meta";
  if (handoff.error && handoff.data === undefined) {
    return <ErrorNotice error={handoff.error} retry={() => void handoff.refetch()} compact />;
  }
  if (handoff.data === undefined) {
    return <p className={`${handoffBox} text-meta text-muted-foreground`}>Loading handoff…</p>;
  }
  if (handoff.data === null) {
    return (
      <p className={`${handoffBox} text-meta text-muted-foreground`}>No handoff written yet.</p>
    );
  }
  const { now, next, blockers, updated_at } = handoff.data;
  return (
    <section className={handoffBox} aria-label="Handoff">
      <p className="text-xs font-semibold text-subtle-foreground">
        Handoff{" "}
        <span className="text-muted-foreground">
          · <RelTime at={updated_at} />
        </span>
      </p>
      <dl
        className={cn(
          "m-0 grid [&_dd]:m-0 [&_dd]:max-w-[72ch] [&_dd]:text-sm [&_dd]:text-foreground [&_dt]:text-meta [&_dt]:text-muted-foreground [&>div]:grid",
          "gap-0.5 [&>div]:gap-2",
          stacked
            ? "[&>div]:grid-cols-[40px_minmax(0,1fr)]"
            : "[&>div]:grid-cols-[72px_minmax(0,1fr)]",
        )}
      >
        <div>
          <dt>Now</dt>
          <dd>
            <PlainText text={now} />
          </dd>
        </div>
        <div>
          <dt>Next</dt>
          <dd>
            <PlainText text={next} />
          </dd>
        </div>
        {blockers && (
          <div>
            <dt>Blockers</dt>
            <dd>
              <PlainText text={blockers} />
            </dd>
          </div>
        )}
      </dl>
    </section>
  );
}

/** How many of a coordinator's turn endings the agents screen shows. */
const SAID_SHOWN = 3;

/** A coordinator's latest turn endings, collapsed until opened. */
function SaidView({ agentId }: { agentId: string }) {
  const said = useQuery(queries.said(agentId, SAID_SHOWN));
  const latest = said.data?.[0];
  const note = "text-muted-foreground mb-1 text-xs";
  return (
    <details className="group/said text-sm">
      <summary className="-mx-1.5 inline-flex min-h-7 cursor-pointer list-none items-center gap-1.5 rounded-sm px-1.5 py-0.5 text-meta font-medium text-subtle-foreground hover:bg-secondary hover:text-foreground [&::-webkit-details-marker]:hidden">
        <Icon
          name="chevron"
          className="size-3.5 transition-transform group-open/said:rotate-90 motion-reduce:transition-none"
        />
        Recent turn endings
        {latest && (
          <>
            {" "}
            · latest <RelTime at={latest.at} />
          </>
        )}
      </summary>
      {said.error && <ErrorNotice error={said.error} retry={() => void said.refetch()} compact />}
      {said.data === undefined ? (
        !said.error && <p className={note}>Loading…</p>
      ) : said.data.length === 0 ? (
        <p className={note}>
          Nothing kept yet. Turn endings are kept while it coordinates a board.
        </p>
      ) : (
        <ul className="mt-1.5 grid gap-1.5">
          {said.data.map((item) => (
            <li key={item.id} className="rounded-sm bg-muted px-3 py-2">
              <p className={note}>
                <Link to="/boards/$slug" params={{ slug: item.board }}>
                  {item.board}
                </Link>{" "}
                · <RelTime at={item.at} />
              </p>
              <CollapsibleMarkdown text={item.text} />
            </li>
          ))}
        </ul>
      )}
    </details>
  );
}
