import { api } from "../api/client.ts";
import type { Agent, BoardEvent, Evidence, Feedback, Task, TaskDetail } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { useResource } from "../live/runtime.ts";
import { Markdown, PlainText, SafeLink, safeHref } from "../text/markdown.tsx";
import {
  AbsTime,
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
import { Icon, StatusIcon } from "../ui/icons.tsx";
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
        <Icon name="chevron" />
        <a href={`/boards/${encodeURIComponent(detail.data?.task.board ?? board)}`}>
          {detail.data?.task.board ?? board}
        </a>
        <Icon name="chevron" />
        <span aria-current="page">{id}</span>
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

  const summary = task.status === "done" ? task.summary : undefined;

  return (
    <article class="task-detail">
      <header class="task-head">
        <h1><PlainText text={task.title} /></h1>
        {/* On narrow screens the properties follow the description; this line keeps the essentials on top. */}
        <p class="task-meta">
          <StatusBadge status={task.status} ready={task.ready} />
          <PriorityBadge priority={task.priority} />
          <Owner task={task} agents={agents} />
        </p>
      </header>

      <div class="task-layout">
        <aside class="task-props" aria-label="Properties">
          <dl class="props">
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
              <dd>
                {task.auto
                  ? "May start without you"
                  : task.body.trimStart().startsWith("Proposal:")
                    ? "Proposal: waits for an agent's review"
                    : "Needs you to start"}
              </dd>
            </div>
            {task.not_before && (
              <div>
                <dt>Not before</dt>
                <dd><AbsTime at={task.not_before} /></dd>
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
              <dd><RelTime at={task.updated_at} /></dd>
            </div>
          </dl>
          <TaskLinks title="Blocked by" ids={task.blockers} related={related} />
          <TaskLinks title="Blocks" ids={detail.blocks} related={related} />
          {task.parent && <TaskLinks title="Parent" ids={[task.parent]} related={related} />}
          <TaskLinks title="Subtasks" ids={detail.children} related={related} />
        </aside>

        <div class="task-body">
          {task.blocked_reason && (
            <div class="notice notice-warn">
              <p class="notice-title">Blocked</p>
              <p><PlainText text={task.blocked_reason} /></p>
            </div>
          )}

          {task.feedback && <FeedbackFacts feedback={task.feedback} agents={agents} related={related} />}

          {summary && (
            <section class="section section-first">
              <h2>Summary</h2>
              <div class="agent-text prose"><Markdown text={summary} /></div>
            </section>
          )}

          <section class={summary ? "section" : "section section-first"}>
            <h2>Description</h2>
            {task.body.trim()
              ? <div class="agent-text prose"><Markdown text={task.body} /></div>
              : <p class="muted">No description.</p>}
          </section>

          <section class="section">
            <h2>Acceptance <span class="section-count">{acceptance.length}</span></h2>
            {acceptance.length > 0
              ? (
                <ul class="acceptance">
                  {acceptance.map((line, index) => <li key={index}><PlainText text={line} /></li>)}
                </ul>
              )
              : <p class="muted">No acceptance checks written.</p>}
          </section>

          <section class="section">
            <h2>Evidence <span class="section-count">{detail.evidence.length}</span></h2>
            {detail.evidence.length > 0
              ? (
                <ul class="list evidence">
                  {detail.evidence.map((item, index) => (
                    <EvidenceItem key={index} item={item} agents={agents} />
                  ))}
                </ul>
              )
              : <p class="muted">None yet. Done requires a check, commit, PR or artifact.</p>}
          </section>

          <section class="section">
            <h2>Notes <span class="section-count">{detail.notes.length}</span></h2>
            {detail.notes.length > 0
              ? (
                <ul class="notes">
                  {detail.notes.map((note) => (
                    <li key={note.id} class="note">
                      <div class="note-meta">
                        <Who id={note.agent} agents={agents} />
                        <span class={`tag kind-${note.kind}`}>{note.kind}</span>
                        <span class="muted"><RelTime at={note.at} /></span>
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
                <ol class="timeline">
                  {[...detail.history].sort((a, b) => b.seq - a.seq).map((event: BoardEvent) => (
                    <EventLine key={event.seq} event={event} agents={agents} showTask={false} />
                  ))}
                </ol>
              )
              : <Empty title="No history" icon="events" />}
          </section>
        </div>
      </div>
    </article>
  );
}

function FeedbackFacts(
  { feedback, agents, related }: {
    feedback: Feedback;
    agents: Map<string, Agent>;
    related: Map<string, Task>;
  },
) {
  const { resolution } = feedback;
  return (
    <section class="section section-first">
      <h2>Feedback</h2>
      <dl class="facts">
        <div>
          <dt>About</dt>
          <dd>
            <code>{feedback.about}</code>
            {feedback.activity && <span class="muted">while {feedback.activity}</span>}
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
            <span class="muted">
              last <RelTime at={feedback.last_reported_at} />
            </span>
          </dd>
        </div>
        <div>
          <dt>Resolution</dt>
          <dd>
            {resolution
              ? (
                <>
                  <span class="tag">{resolution.kind}</span>
                  {resolution.task && <TaskRef id={resolution.task} related={related} />}
                  {resolution.reason && <PlainText text={resolution.reason} />}
                  <span class="muted">
                    by {resolution.by === "user" ? "you" : <Who id={resolution.by} agents={agents} />}{" "}
                    <RelTime at={resolution.at} />
                  </span>
                </>
              )
              : <span class="muted">Unresolved</span>}
          </dd>
        </div>
        {feedback.recurs && (
          <div>
            <dt>Recurs</dt>
            <dd>
              <TaskRef id={feedback.recurs} related={related} />
              <span class="muted">came back after that fix</span>
            </dd>
          </div>
        )}
      </dl>
    </section>
  );
}

/** A link to a task, with its status and title when it is on this board. */
function TaskRef({ id, related }: { id: string; related: Map<string, Task> }) {
  const task = related.get(id);
  return (
    <>
      <a href={`/tasks/${encodeURIComponent(id)}`}><code>{id}</code></a>
      {task && <StatusBadge status={task.status} ready={task.ready} />}
    </>
  );
}

function TaskLinks(
  { title, ids, related }: { title: string; ids: string[]; related: Map<string, Task> },
) {
  if (ids.length === 0) return null;
  return (
    <section class="prop-group">
      <h2>{title}</h2>
      <ul class="link-list">
        {ids.map((id) => {
          const task = related.get(id);
          return (
            <li key={id}>
              <a class="link-card" href={`/tasks/${encodeURIComponent(id)}`}>
                {task && <StatusIcon status={task.status === "open" && task.ready ? "ready" : task.status} label />}
                <code>{id}</code>
                {task && <PlainText text={task.title} class="link-card-title" />}
              </a>
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
      <span class="tag">{item.kind}</span>
      <span class="evidence-value">
        {safeHref(item.value) ? <SafeLink href={item.value}>{item.value}</SafeLink> : <code>{item.value}</code>}
      </span>
      {item.exit_code !== null && (
        <span class={item.exit_code === 0 ? "tag tag-ok" : "tag tag-bad"}>
          exit {item.exit_code}
        </span>
      )}
      <span class="muted">
        <Who id={item.agent} agents={agents} /> · <RelTime at={item.at} />
      </span>
    </li>
  );
}
