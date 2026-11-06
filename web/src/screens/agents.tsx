import { useLocation } from "preact-iso";
import { api } from "../api/client.ts";
import type { Agent, Task } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { useResource } from "../live/runtime.ts";
import { PlainText } from "../text/markdown.tsx";
import {
  AgentStateBadge,
  Empty,
  ErrorNotice,
  Query,
  RelTime,
  useAgents,
  useTitle,
} from "../ui/common.tsx";

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
    .filter((agent) => (state === "all" ? true : state ? agent.state === state : agent.state !== "ended"))
    .sort((a, b) =>
      STATE_ORDER[a.state] - STATE_ORDER[b.state] || Date.parse(b.last_seen) - Date.parse(a.last_seen)
    );
}

export function AgentsScreen() {
  useTitle("Agents");
  const location = useLocation();
  const state = location.query.state ?? "";
  const agents = useAgents();
  const doing = useResource("tasks:doing", [tags.tasks], () => api.tasks({ status: "doing" }));
  const current = new Map<string, Task>();
  for (const task of doing.data ?? []) if (task.owner) current.set(task.owner, task);

  return (
    <>
      <header class="page-head">
        <h1>Agents</h1>
      </header>
      <nav class="segmented" aria-label="Filter agents by state">
        {FILTERS.map((filter) => (
          <a
            key={filter.value}
            href={filter.value ? `/agents?state=${filter.value}` : "/agents"}
            aria-current={state === filter.value ? "page" : undefined}
          >
            {filter.label}
          </a>
        ))}
      </nav>
      {doing.error && <ErrorNotice error={doing.error} retry={doing.retry} compact />}
      <Query resource={agents} label="Loading agents">
        {(list) => {
          const shown = sortAgents(list, state);
          if (shown.length === 0) {
            return (
              <Empty title={list.length === 0 ? "No agents have registered" : "No agents in this state"}>
                {list.length === 0 && "Agents register when a session with the board hooks starts."}
              </Empty>
            );
          }
          return (
            <div class="agent-grid">
              {shown.map((agent) => (
                <AgentCard key={agent.id} agent={agent} task={current.get(agent.id)} />
              ))}
            </div>
          );
        }}
      </Query>
    </>
  );
}

function AgentCard({ agent, task }: { agent: Agent; task: Task | undefined }) {
  return (
    <article class="card agent-card">
      <header class="card-head">
        <h2 class="card-title handle" title={agent.id}>{agent.handle}</h2>
        <AgentStateBadge agent={agent} />
        {agent.role === "coordinator" && <span class="chip">coordinator</span>}
      </header>
      {agent.title && <p class="agent-title"><PlainText text={agent.title} /></p>}
      <dl class="facts facts-compact">
        <div>
          <dt>Device</dt>
          <dd>{agent.device} · {agent.runtime}</dd>
        </div>
        {agent.repo && (
          <div>
            <dt>Repo</dt>
            <dd class="wrap">
              <code>{agent.repo}</code>
              {agent.branch && <> · <code>{agent.branch}</code></>}
            </dd>
          </div>
        )}
        <div>
          <dt>Working on</dt>
          <dd>
            {task
              ? (
                <a href={`/tasks/${encodeURIComponent(task.id)}`}>
                  <code>{task.id}</code> <PlainText text={task.title} />
                </a>
              )
              : <span class="muted">no task in doing</span>}
          </dd>
        </div>
        <div>
          <dt>Last seen</dt>
          <dd><RelTime at={agent.last_seen} /></dd>
        </div>
      </dl>
      {agent.state !== "ended" && <HandoffView agentId={agent.id} />}
    </article>
  );
}

function HandoffView({ agentId }: { agentId: string }) {
  const handoff = useResource(
    `handoff:${agentId}`,
    [tags.handoff(agentId)],
    () => api.handoff(agentId),
  );
  if (handoff.error && handoff.data === undefined) {
    return <ErrorNotice error={handoff.error} retry={handoff.retry} compact />;
  }
  if (handoff.data === undefined) return <p class="muted handoff">Loading handoff…</p>;
  if (handoff.data === null) return <p class="muted handoff">No handoff written.</p>;
  const { now, next, blockers, updated_at } = handoff.data;
  return (
    <section class="handoff" aria-label="Handoff">
      <p class="handoff-head">
        Handoff · <RelTime at={updated_at} />
      </p>
      <dl class="handoff-fields">
        <div>
          <dt>Now</dt>
          <dd class="agent-text"><PlainText text={now} /></dd>
        </div>
        <div>
          <dt>Next</dt>
          <dd class="agent-text"><PlainText text={next} /></dd>
        </div>
        {blockers && (
          <div>
            <dt>Blockers</dt>
            <dd class="agent-text"><PlainText text={blockers} /></dd>
          </div>
        )}
      </dl>
    </section>
  );
}
