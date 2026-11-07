import { Fragment } from "preact";
import { useLocation } from "preact-iso";
import { api } from "../api/client.ts";
import type { Agent, Task } from "../api/schema.ts";
import { tags } from "../live/refresh.ts";
import { useNow, useResource } from "../live/runtime.ts";
import { PlainText } from "../text/markdown.tsx";
import {
  AbsTime,
  agentIndex,
  Empty,
  Owner,
  PriorityBadge,
  Query,
  RelTime,
  StatusBadge,
  useAgents,
  useTitle,
} from "../ui/common.tsx";

const DONE_WINDOW_MS = 30 * 86_400_000;

export interface TaskFilters {
  q: string;
  priority: string;
  /** An agent id, `none` for unowned, or empty for anyone. */
  owner: string;
  readyOnly: boolean;
}

export interface TaskGroup {
  key: "open" | "doing" | "blocked" | "review" | "done";
  title: string;
  tasks: Task[];
}

/**
 * Group a board's tasks for display: open (ready first), doing, blocked, review, and done
 * in the last 30 days. Dropped tasks are not shown. Exported for tests.
 */
export function groupTasks(tasks: Task[], filters: TaskFilters, now: number): TaskGroup[] {
  const q = filters.q.trim().toLowerCase();
  const visible = tasks.filter((task) => {
    if (task.status === "dropped") return false;
    if (task.status === "done" && now - Date.parse(task.updated_at) > DONE_WINDOW_MS) return false;
    if (filters.priority && task.priority !== filters.priority) return false;
    if (filters.owner === "none" ? task.owner !== null : filters.owner && task.owner !== filters.owner) {
      return false;
    }
    if (filters.readyOnly && !task.ready) return false;
    return !q || task.title.toLowerCase().includes(q) || task.id.toLowerCase().includes(q);
  });
  const of = (status: Task["status"]) => visible.filter((task) => task.status === status);
  const open = of("open").sort((a, b) => Number(b.ready) - Number(a.ready));
  const done = of("done").sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
  return [
    { key: "open", title: "Open", tasks: open },
    { key: "doing", title: "Doing", tasks: of("doing") },
    { key: "blocked", title: "Blocked", tasks: of("blocked") },
    { key: "review", title: "Review", tasks: of("review") },
    { key: "done", title: "Done, last 30 days", tasks: done },
  ];
}

export function BoardScreen({ slug }: { slug: string }) {
  const location = useLocation();
  const now = useNow(60_000);
  const board = useResource(`board:${slug}`, [tags.board(slug), tags.boards], () => api.board(slug));
  const tasks = useResource(`tasks:${slug}`, [tags.board(slug)], () => api.tasks({ board: slug }));
  const agentList = useAgents().data;
  const agents = agentIndex(agentList);
  useTitle(board.data?.title ?? slug);

  const filters: TaskFilters = {
    q: location.query.q ?? "",
    priority: location.query.priority ?? "",
    owner: location.query.owner ?? "",
    readyOnly: location.query.ready === "1",
  };
  const setFilter = (key: string, value: string) => {
    const params = new URLSearchParams(location.query);
    if (value) params.set(key, value);
    else params.delete(key);
    const search = params.toString();
    location.route(`${location.path}${search ? `?${search}` : ""}`, true);
  };
  const filtered = filters.q || filters.priority || filters.owner || filters.readyOnly;

  return (
    <>
      <nav class="crumbs" aria-label="Breadcrumb">
        <a href="/">Boards</a>
      </nav>
      <header class="page-head">
        <h1>{board.data?.title ?? slug}</h1>
        <code class="muted">{slug}</code>
        {board.data?.autopilot && <span class="badge tone-doing">Autopilot</span>}
      </header>
      {board.data?.goal && <p class="lede"><PlainText text={board.data.goal} /></p>}

      <form class="filters" role="search" onSubmit={(event) => event.preventDefault()}>
        <label class="field field-grow">
          <span>Search</span>
          <input
            type="search"
            value={filters.q}
            placeholder="Title or id"
            onInput={(event) => setFilter("q", event.currentTarget.value)}
          />
        </label>
        <label class="field">
          <span>Priority</span>
          <select
            value={filters.priority}
            onChange={(event) => setFilter("priority", event.currentTarget.value)}
          >
            <option value="">Any</option>
            <option value="p1">P1</option>
            <option value="p2">P2</option>
            <option value="p3">P3</option>
          </select>
        </label>
        <label class="field">
          <span>Owner</span>
          <select
            value={filters.owner}
            onChange={(event) => setFilter("owner", event.currentTarget.value)}
          >
            <option value="">Anyone</option>
            <option value="none">Unowned</option>
            {ownerOptions(tasks.data ?? [], agents).map((agent) => (
              <option key={agent.id} value={agent.id}>{agent.label}</option>
            ))}
          </select>
        </label>
        <label class="check">
          <input
            type="checkbox"
            checked={filters.readyOnly}
            onChange={(event) => setFilter("ready", event.currentTarget.checked ? "1" : "")}
          />
          Ready only
        </label>
      </form>

      <Query resource={tasks} label="Loading tasks">
        {(list) => {
          const done = new Set(list.filter((task) => task.status === "done").map((task) => task.id));
          const groups = groupTasks(list, filters, now).filter((group) => group.tasks.length > 0);
          if (groups.length === 0) {
            return filtered
              ? <Empty title="No tasks match these filters" />
              : (
                <Empty title="No tasks on this board yet">
                  Agents add tasks with <code>task_add</code>.
                </Empty>
              );
          }
          return groups.map((group) => (
            <section key={group.key} class="task-group" aria-labelledby={`group-${group.key}`}>
              <h2 id={`group-${group.key}`} class="group-title">
                {group.title} <span class="group-count">{group.tasks.length}</span>
              </h2>
              <ul class="task-list">
                {group.tasks.map((task) => (
                  <TaskRow key={task.id} task={task} agents={agents} done={done} now={now} />
                ))}
              </ul>
            </section>
          ));
        }}
      </Query>
    </>
  );
}

function ownerOptions(tasks: Task[], agents: Map<string, Agent>) {
  const ids = [...new Set(tasks.flatMap((task) => (task.owner ? [task.owner] : [])))];
  return ids
    .map((id) => ({ id, label: agents.get(id)?.handle ?? id }))
    .sort((a, b) => a.label.localeCompare(b.label));
}

function TaskRow(
  { task, agents, done, now }: {
    task: Task;
    agents: Map<string, Agent>;
    done: ReadonlySet<string>;
    now: number;
  },
) {
  // Blockers already done no longer hold the task up.
  const blockers = task.blockers.filter((id) => !done.has(id));
  // Only a date still ahead holds an open task up.
  const waitsUntil = task.status === "open" && task.not_before !== null &&
      Date.parse(task.not_before) > now
    ? task.not_before
    : null;
  return (
    <li class="task-row">
      <div class="task-line">
        <PriorityBadge priority={task.priority} />
        <a class="task-title" href={`/tasks/${encodeURIComponent(task.id)}`}>
          <code class="task-id">{task.id}</code> <PlainText text={task.title} />
        </a>
      </div>
      <div class="task-meta">
        <StatusBadge status={task.status} ready={task.ready} />
        <Owner task={task} agents={agents} />
        {blockers.length > 0 && (
          <span class="blockers">
            blocked by{" "}
            {blockers.map((id, index) => (
              <Fragment key={id}>
                {index > 0 && ", "}
                <a href={`/tasks/${encodeURIComponent(id)}`}><code>{id}</code></a>
              </Fragment>
            ))}
          </span>
        )}
        {task.auto && <span class="chip">auto</span>}
        {waitsUntil && (
          <span class="chip">
            waits until <AbsTime at={waitsUntil} />
          </span>
        )}
        <span class="muted">
          updated <RelTime at={task.updated_at} />
        </span>
      </div>
      {task.blocked_reason && (
        <p class="blocked-reason">
          <PlainText text={task.blocked_reason} />
        </p>
      )}
    </li>
  );
}
