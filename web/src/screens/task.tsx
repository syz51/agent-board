import { api } from "../api/client.ts";
import type { Agent, BoardEvent, Evidence, Task, TaskDetail } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { useResource } from "../live/runtime.ts";
import { Markdown, PlainText, SafeLink, safeHref } from "../text/markdown.tsx";
import {
  agentIndex,
  boardOfTask,
  Empty,
  Owner,
  PriorityBadge,
  Query,
  RelTime,
  StatusBadge,
  useAgents,
  useTitle,
  Who,
} from "../ui/common.tsx";
import { EventLine } from "./events.tsx";

export function TaskScreen({ id }: { id: string }) {
  const board = boardOfTask(id);
  const detail = useResource(`task:${id}`, [tags.task(id), tags.board(board)], () => api.task(id));
  const agents = agentIndex(useAgents().data);
  // Titles and statuses of blockers, subtasks and the parent (shared with the board screen).
  const boardTasks = useResource(`tasks:${board}`, [tags.board(board)], () => api.tasks({ board }));
  const related = new Map((boardTasks.data ?? []).map((task) => [task.id, task]));
  useTitle(detail.data ? `${detail.data.task.id} ${detail.data.task.title}` : id);

  return (
    <>
      <nav class="crumbs" aria-label="Breadcrumb">
        <a href="/">Boards</a>
        <span aria-hidden="true">/</span>
        <a href={`/boards/${encodeURIComponent(detail.data?.task.board ?? board)}`}>
          {detail.data?.task.board ?? board}
        </a>
      </nav>
      <Query resource={detail} label="Loading task">
        {(data) => <TaskView detail={data} agents={agents} related={related} />}
      </Query>
    </>
  );
}

function TaskView(
  { detail, agents, related }: {
    detail: TaskDetail;
    agents: Map<string, Agent>;
    related: Map<string, Task>;
  },
) {
  const { task } = detail;
  const acceptance = task.acceptance.split("\n").map((line) => line.trim()).filter(Boolean);

  return (
    <article class="task-detail">
      <header class="page-head task-head">
        <code class="task-id">{task.id}</code>
        <h1><PlainText text={task.title} /></h1>
      </header>

      <dl class="facts">
        <div>
          <dt>Status</dt>
          <dd>
            <StatusBadge status={task.status} ready={task.ready} />
            {task.status === "open" && !task.ready && task.blockers.length > 0 && (
              <span class="muted">waiting on blockers</span>
            )}
          </dd>
        </div>
        <div>
          <dt>Priority</dt>
          <dd><PriorityBadge priority={task.priority} /></dd>
        </div>
        <div>
          <dt>Owner</dt>
          <dd>
            <Owner task={task} agents={agents} />
          </dd>
        </div>
        <div>
          <dt>Autopilot</dt>
          <dd>{task.auto ? "May start without you" : "Needs you to start"}</dd>
        </div>
        <div>
          <dt>Created</dt>
          <dd>
            <RelTime at={task.created_at} /> by <Who id={task.created_by} agents={agents} />
          </dd>
        </div>
        <div>
          <dt>Updated</dt>
          <dd><RelTime at={task.updated_at} /></dd>
        </div>
      </dl>

      {task.blocked_reason && (
        <div class="notice notice-warn">
          <p class="notice-title">Blocked</p>
          <p><PlainText text={task.blocked_reason} /></p>
        </div>
      )}

      <TaskLinks title="Blocked by" ids={task.blockers} related={related} />
      <TaskLinks title="Blocks" ids={detail.blocks} related={related} />
      {task.parent && <TaskLinks title="Parent" ids={[task.parent]} related={related} />}
      <TaskLinks title="Subtasks" ids={detail.children} related={related} />

      <section class="section">
        <h2>Description</h2>
        {task.body.trim()
          ? <div class="agent-text"><Markdown text={task.body} /></div>
          : <p class="muted">No description.</p>}
      </section>

      <section class="section">
        <h2>Acceptance</h2>
        {acceptance.length > 0
          ? (
            <ul class="acceptance">
              {acceptance.map((line, index) => <li key={index}><PlainText text={line} /></li>)}
            </ul>
          )
          : <p class="muted">No acceptance checks written.</p>}
      </section>

      <section class="section">
        <h2>Evidence</h2>
        {detail.evidence.length > 0
          ? (
            <ul class="evidence">
              {detail.evidence.map((item, index) => (
                <EvidenceItem key={index} item={item} agents={agents} />
              ))}
            </ul>
          )
          : <p class="muted">None yet. Done requires a check, commit, PR or artifact.</p>}
      </section>

      <section class="section">
        <h2>Notes</h2>
        {detail.notes.length > 0
          ? (
            <ul class="notes">
              {detail.notes.map((note) => (
                <li key={note.id} class="note">
                  <div class="note-meta">
                    <span class={`chip kind-${note.kind}`}>{note.kind}</span>
                    <Who id={note.agent} agents={agents} />
                    <RelTime at={note.at} />
                  </div>
                  <p class="agent-text"><PlainText text={note.text} /></p>
                </li>
              ))}
            </ul>
          )
          : <p class="muted">No notes.</p>}
      </section>

      <section class="section">
        <h2>History</h2>
        {detail.history.length > 0
          ? (
            <ol class="event-list">
              {[...detail.history].sort((a, b) => b.seq - a.seq).map((event: BoardEvent) => (
                <EventLine key={event.seq} event={event} agents={agents} showTask={false} />
              ))}
            </ol>
          )
          : <Empty title="No history" />}
      </section>
    </article>
  );
}

function TaskLinks(
  { title, ids, related }: { title: string; ids: string[]; related: Map<string, Task> },
) {
  if (ids.length === 0) return null;
  return (
    <section class="section section-tight">
      <h2>{title}</h2>
      <ul class="link-list">
        {ids.map((id) => {
          const task = related.get(id);
          return (
            <li key={id}>
              <a href={`/tasks/${encodeURIComponent(id)}`}><code>{id}</code></a>
              {task && (
                <>
                  <StatusBadge status={task.status} ready={task.ready} />
                  <PlainText text={task.title} />
                </>
              )}
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function EvidenceItem({ item, agents }: { item: Evidence; agents: Map<string, Agent> }) {
  return (
    <li class="evidence-item">
      <span class="chip">{item.kind}</span>
      <span class="evidence-value">
        {safeHref(item.value) ? <SafeLink href={item.value}>{item.value}</SafeLink> : <code>{item.value}</code>}
      </span>
      {item.exit_code !== null && (
        <span class={item.exit_code === 0 ? "badge tone-done" : "badge tone-blocked"}>
          exit {item.exit_code}
        </span>
      )}
      <span class="muted">
        <Who id={item.agent} agents={agents} /> · <RelTime at={item.at} />
      </span>
    </li>
  );
}
