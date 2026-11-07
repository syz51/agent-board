import { useQuery } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import type { Agent, Task } from "@/api/schema.ts";
import { queries } from "@/live/query.ts";
import { PlainText } from "@/text/markdown.tsx";
import {
  AgentStateBadge,
  Empty,
  ErrorNotice,
  PageHeader,
  QueryFrame,
  RelTime,
  useAgents,
  useTitle,
} from "@/ui/common.tsx";
import { Avatar, StatusIcon } from "@/ui/icons.tsx";
import { CollapsibleMarkdown } from "./updates.tsx";

const route = getRouteApi("/agents");

const STATE_ORDER: Record<Agent["state"], number> = { running: 0, paused: 1, idle: 2, ended: 3 };
const FILTERS = [
  { value: "", label: "Active" },
  { value: "running", label: "Running" },
  { value: "paused", label: "Paused" },
  { value: "idle", label: "Idle" },
  { value: "ended", label: "Ended" },
  { value: "all", label: "All" },
] as const;

/** Agents for a state filter (`""` hides ended ones), busiest first. Exported for tests. */
export function sortAgents(agents: Agent[], state: string): Agent[] {
  return agents
    .filter((agent) =>
      state === "all" ? true : state ? agent.state === state : agent.state !== "ended",
    )
    .sort(
      (a, b) =>
        STATE_ORDER[a.state] - STATE_ORDER[b.state] ||
        Date.parse(b.last_seen) - Date.parse(a.last_seen),
    );
}

export function AgentsScreen() {
  useTitle("Agents");
  const state = route.useSearch().state ?? "";
  const agents = useAgents();
  const doing = useQuery(queries.doingTasks());
  const current = new Map<string, Task>();
  for (const task of doing.data ?? []) if (task.owner) current.set(task.owner, task);
  const count = (state: Agent["state"]) =>
    agents.data?.filter((agent) => agent.state === state).length ?? 0;

  return (
    <>
      <PageHeader
        title="Agents"
        sub={
          agents.data && (
            <>
              {count("running")} running · {count("paused")} paused · {count("idle")} idle
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
          const shown = sortAgents(list, state);
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
            <div className="grid gap-3">
              {shown.map((agent) => (
                <AgentCard key={agent.id} agent={agent} task={current.get(agent.id)} />
              ))}
            </div>
          );
        }}
      </QueryFrame>
    </>
  );
}

function AgentCard({ agent, task }: { agent: Agent; task: Task | undefined }) {
  return (
    <article className="grid gap-3.5 rounded-lg border bg-card px-4 py-3.5 shadow-raise lg:grid-cols-[248px_minmax(0,1fr)] lg:gap-6">
      <div className="grid content-start justify-items-start gap-2">
        {/* The avatar beside the handle, with the state on its own line under it. */}
        <header className="grid grid-cols-[auto_minmax(0,1fr)] items-center justify-items-start gap-x-2.5 gap-y-0.5">
          <Avatar handle={agent.handle} large className="row-span-2 size-9 text-xs" />
          <h2
            className="text-base font-semibold tracking-[-0.011em] [font-variant-numeric:tabular-nums_slashed-zero]"
            title={agent.id}
          >
            {agent.handle}
          </h2>
          <AgentStateBadge agent={agent} />
        </header>
        <dl className="m-0 grid gap-0.5 text-meta leading-5 [&_code]:text-xs [&_dd]:m-0 [&_dd]:text-subtle-foreground [&_dt]:text-muted-foreground [&>div]:grid [&>div]:grid-cols-[72px_minmax(0,1fr)] [&>div]:gap-2">
          {agent.role === "coordinator" && (
            <div>
              <dt>Role</dt>
              <dd>Coordinator</dd>
            </div>
          )}
          <div>
            <dt>Device</dt>
            <dd>
              {agent.device} · {agent.runtime}
            </dd>
          </div>
          {agent.repo && (
            <div>
              <dt>Repo</dt>
              <dd className="[overflow-wrap:anywhere]">
                <code>{agent.repo.replace(/^github\.com\//, "")}</code>
              </dd>
            </div>
          )}
          {agent.branch && (
            <div>
              <dt>Branch</dt>
              <dd className="[overflow-wrap:anywhere]">
                <code>{agent.branch}</code>
              </dd>
            </div>
          )}
          <div>
            <dt>Last seen</dt>
            <dd>
              <RelTime at={agent.last_seen} />
            </dd>
          </div>
        </dl>
      </div>
      <div className="grid min-w-0 content-start gap-2.5">
        <p className="font-medium">
          {task ? (
            <Link
              className="inline-flex flex-wrap items-baseline gap-x-2 gap-y-1 text-foreground"
              to="/tasks/$id"
              params={{ id: task.id }}
            >
              <StatusIcon status="doing" className="self-center" />
              <span className="ident text-meta text-muted-foreground">{task.id}</span>{" "}
              <PlainText className="whitespace-normal" text={task.title} />
            </Link>
          ) : agent.title ? (
            <PlainText className="whitespace-normal" text={agent.title} />
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

const handoffBox = "bg-muted grid gap-1.5 rounded-sm border px-3 py-2.5";

function HandoffView({ agentId }: { agentId: string }) {
  const handoff = useQuery(queries.handoff(agentId));
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
      <dl className="m-0 grid gap-1 [&_dd]:m-0 [&_dd]:max-w-[72ch] [&_dt]:text-meta [&_dt]:text-muted-foreground [&>div]:grid [&>div]:grid-cols-[64px_minmax(0,1fr)] [&>div]:gap-2.5">
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
    <details className="text-sm">
      <summary className="min-h-7 cursor-pointer py-0.5 text-meta text-muted-foreground hover:text-foreground">
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
