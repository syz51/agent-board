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
import { Avatar, StatusIcon } from "../ui/icons.tsx";
import { CollapsibleMarkdown } from "./updates.tsx";

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
        <div class="page-title">
          <h1>Agents</h1>
          {agents.data && (
            <p class="page-sub">
              {agents.data.filter((agent) => agent.state === "running").length} running ·{" "}
              {agents.data.filter((agent) => agent.state === "paused").length} paused ·{" "}
              {agents.data.filter((agent) => agent.state === "idle").length} idle
            </p>
          )}
        </div>
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
            <div class="agent-list">
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
      <div class="agent-who">
        <header class="agent-head">
          <Avatar handle={agent.handle} large />
          <h2 class="agent-handle" title={agent.id}>{agent.handle}</h2>
          <AgentStateBadge agent={agent} />
        </header>
        <dl class="agent-facts">
          {agent.role === "coordinator" && (
            <div>
              <dt>Role</dt>
              <dd>Coordinator</dd>
            </div>
          )}
          <div>
            <dt>Device</dt>
            <dd>{agent.device} · {agent.runtime}</dd>
          </div>
          {agent.repo && (
            <div>
              <dt>Repo</dt>
              <dd class="wrap"><code>{agent.repo.replace(/^github\.com\//, "")}</code></dd>
            </div>
          )}
          {agent.branch && (
            <div>
              <dt>Branch</dt>
              <dd class="wrap"><code>{agent.branch}</code></dd>
            </div>
          )}
          <div>
            <dt>Last seen</dt>
            <dd><RelTime at={agent.last_seen} /></dd>
          </div>
        </dl>
      </div>
      <div class="agent-work">
        <p class="agent-task">
          {task
            ? (
              <a href={`/tasks/${encodeURIComponent(task.id)}`}>
                <StatusIcon status="doing" />
                <code>{task.id}</code> <PlainText text={task.title} />
              </a>
            )
            : agent.title
            ? <span class="agent-title"><PlainText text={agent.title} /></span>
            : <span class="muted">No task in doing</span>}
        </p>
        {agent.state !== "ended" && <HandoffView agentId={agent.id} />}
        {agent.role === "coordinator" && <SaidView agentId={agent.id} />}
      </div>
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
  if (handoff.data === undefined) return <p class="muted handoff handoff-empty">Loading handoff…</p>;
  if (handoff.data === null) return <p class="muted handoff handoff-empty">No handoff written yet.</p>;
  const { now, next, blockers, updated_at } = handoff.data;
  return (
    <section class="handoff" aria-label="Handoff">
      <p class="handoff-head">
        Handoff <span class="muted">· <RelTime at={updated_at} /></span>
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

/** How many of a coordinator's turn endings the agents screen shows. */
const SAID_SHOWN = 3;

/** A coordinator's latest turn endings, collapsed until opened. */
function SaidView({ agentId }: { agentId: string }) {
  const said = useResource(`said:${agentId}`, [tags.updates], () => api.agentSaid(agentId, SAID_SHOWN));
  const latest = said.data?.[0];
  return (
    <details class="said">
      <summary>
        Recent turn endings
        {latest && <> · latest <RelTime at={latest.at} /></>}
      </summary>
      {said.error && <ErrorNotice error={said.error} retry={said.retry} compact />}
      {said.data === undefined
        ? !said.error && <p class="muted said-head">Loading…</p>
        : said.data.length === 0
        ? <p class="muted said-head">Nothing kept yet. Turn endings are kept while it coordinates a board.</p>
        : (
          <ul class="said-list">
            {said.data.map((item) => (
              <li key={item.id} class="said-item">
                <p class="said-head">
                  <a href={`/boards/${encodeURIComponent(item.board)}`}>{item.board}</a> · <RelTime at={item.at} />
                </p>
                <CollapsibleMarkdown text={item.text} />
              </li>
            ))}
          </ul>
        )}
    </details>
  );
}
