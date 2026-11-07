import { useQuery } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import { createColumnHelper, tableFeatures, useTable } from "@tanstack/react-table";
import { Fragment, type ReactNode, useMemo } from "react";
import type { Agent, Task } from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { NativeSelectOption } from "@/components/ui/native-select.tsx";
import { queries } from "@/live/query.ts";
import { useNow } from "@/live/runtime.ts";
import type { BoardSearch } from "@/router.tsx";
import { PlainText } from "@/text/markdown.tsx";
import {
  AbsTime,
  agentIndex,
  Crumbs,
  Empty,
  Owner,
  PageHeader,
  PriorityBadge,
  QueryFrame,
  RelTime,
  useAgents,
  useTitle,
} from "@/ui/common.tsx";
import { FilterSelect, FilterToggle, SearchField } from "@/ui/filters.tsx";
import { Icon, StatusIcon } from "@/ui/icons.tsx";

const route = getRouteApi("/boards/$slug");

const DONE_WINDOW_MS = 30 * 86_400_000;

export interface TaskFilters {
  q: string;
  priority: string;
  /** An agent id, `none` for unowned, or empty for anyone. */
  owner: string;
  readyOnly: boolean;
}

export interface TaskGroup {
  key: "ready" | "open" | "doing" | "blocked" | "review" | "done";
  title: string;
  tasks: Task[];
}

/**
 * Group a board's tasks for display: doing, review, blocked, ready, open tasks still
 * waiting, and done in the last 30 days. Dropped tasks are not shown. Exported for tests.
 */
export function groupTasks(tasks: Task[], filters: TaskFilters, now: number): TaskGroup[] {
  const q = filters.q.trim().toLowerCase();
  const visible = tasks.filter((task) => {
    if (task.status === "dropped") return false;
    if (task.status === "done" && now - Date.parse(task.updated_at) > DONE_WINDOW_MS) return false;
    if (filters.priority && task.priority !== filters.priority) return false;
    if (
      filters.owner === "none" ? task.owner !== null : filters.owner && task.owner !== filters.owner
    ) {
      return false;
    }
    if (filters.readyOnly && !task.ready) return false;
    return !q || task.title.toLowerCase().includes(q) || task.id.toLowerCase().includes(q);
  });
  const of = (status: Task["status"]) => visible.filter((task) => task.status === status);
  const open = of("open");
  const done = of("done").sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
  return [
    { key: "doing", title: "Doing", tasks: of("doing") },
    { key: "review", title: "Review", tasks: of("review") },
    { key: "blocked", title: "Blocked", tasks: of("blocked") },
    { key: "ready", title: "Ready", tasks: open.filter((task) => task.ready) },
    { key: "open", title: "Waiting", tasks: open.filter((task) => !task.ready) },
    { key: "done", title: "Done in the last 30 days", tasks: done },
  ];
}

/** The filters a board's URL holds. */
export function filtersOf(search: BoardSearch): TaskFilters {
  return {
    q: search.q ?? "",
    priority: search.priority ?? "",
    owner: search.owner ?? "",
    readyOnly: search.ready === "1",
  };
}

export function BoardScreen() {
  const { slug } = route.useParams();
  const search = route.useSearch();
  const navigate = route.useNavigate();
  const now = useNow(60_000);
  // The board comes from the list the sidebar already holds: one request for both.
  const boards = useQuery(queries.boards());
  const board = boards.data?.find((item) => item.slug === slug);
  const tasks = useQuery(queries.boardTasks(slug));
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);
  // One set per task list, so the memoized rows keep their identity between renders.
  const done = useMemo(
    () =>
      new Set((tasks.data ?? []).filter((task) => task.status === "done").map((task) => task.id)),
    [tasks.data],
  );
  useTitle(board?.title ?? slug);

  const filters = filtersOf(search);
  const setFilter = (key: keyof BoardSearch, value: string) =>
    void navigate({
      search: { ...search, [key]: value || undefined },
      replace: true,
    });
  const filtered = filters.q || filters.priority || filters.owner || filters.readyOnly;

  return (
    <>
      <Crumbs current={board?.title ?? slug}>
        <Link to="/">Boards</Link>
      </Crumbs>
      <PageHeader title={board?.title ?? slug} sub={board?.goal && <PlainText text={board.goal} />}>
        {board && board.title !== slug && (
          <span className="ident text-meta text-muted-foreground">{slug}</span>
        )}
        {board?.autopilot && (
          <Badge variant="accent" shape="tag">
            <Icon name="bolt" />
            Autopilot
          </Badge>
        )}
        {board?.temporary && (
          <Badge variant="outline" shape="tag">
            Temporary
          </Badge>
        )}
      </PageHeader>
      {board?.temporary && (
        <p className="-mt-3 mb-5 text-meta text-muted-foreground">
          Temporary: the server closes this board once it is idle, and deletes it some time after
          that.
        </p>
      )}

      {/* No filters on a board without tasks; disabled until its tasks have loaded. */}
      {(filtered || tasks.data === undefined || tasks.data.length > 0) && (
        <form
          className="mb-5 flex flex-wrap items-center gap-2 max-md:grid max-md:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto]"
          role="search"
          onSubmit={(event) => event.preventDefault()}
        >
          <fieldset className="contents" disabled={tasks.data === undefined}>
            <SearchField
              label="Search tasks"
              placeholder="Search by title or id"
              value={filters.q}
              onChange={(value) => setFilter("q", value)}
              className="max-md:col-span-2"
            />
            <FilterSelect
              label="Priority"
              value={filters.priority}
              onChange={(value) => setFilter("priority", value)}
            >
              <NativeSelectOption value="">Any</NativeSelectOption>
              <NativeSelectOption value="p1">P1</NativeSelectOption>
              <NativeSelectOption value="p2">P2</NativeSelectOption>
              <NativeSelectOption value="p3">P3</NativeSelectOption>
            </FilterSelect>
            <FilterSelect
              label="Owner"
              value={filters.owner}
              onChange={(value) => setFilter("owner", value)}
              className="max-md:col-span-2"
            >
              <NativeSelectOption value="">Anyone</NativeSelectOption>
              <NativeSelectOption value="none">Unowned</NativeSelectOption>
              {ownerOptions(tasks.data ?? [], agents).map((agent) => (
                <NativeSelectOption key={agent.id} value={agent.id}>
                  {agent.label}
                </NativeSelectOption>
              ))}
            </FilterSelect>
            <FilterToggle
              checked={filters.readyOnly}
              onChange={(checked) => setFilter("ready", checked ? "1" : "")}
              className="max-md:col-start-3 max-md:row-start-1"
            >
              Ready only
            </FilterToggle>
          </fieldset>
        </form>
      )}

      <QueryFrame query={tasks} label="Loading tasks">
        {(list) => {
          const groups = groupTasks(list, filters, now).filter((group) => group.tasks.length > 0);
          if (groups.length === 0) {
            return filtered ? (
              <Empty title="No tasks match these filters" icon="search" />
            ) : (
              <Empty title="No tasks on this board yet" icon="task">
                Agents add tasks with <code>task_add</code>; they appear here as they do.
              </Empty>
            );
          }
          return (
            <TaskBoard>
              {groups.map((group) => (
                <TaskGroupSection
                  key={group.key}
                  id={`group-${group.key}`}
                  heading={
                    <>
                      <StatusIcon status={group.key} />
                      {group.title}
                    </>
                  }
                  count={group.tasks.length}
                >
                  <TaskList tasks={group.tasks} agents={agents} done={done} now={now} />
                </TaskGroupSection>
              ))}
            </TaskBoard>
          );
        }}
      </QueryFrame>
    </>
  );
}

function ownerOptions(tasks: Task[], agents: Map<string, Agent>) {
  const ids = [...new Set(tasks.flatMap((task) => (task.owner ? [task.owner] : [])))];
  return ids
    .map((id) => ({ id, label: agents.get(id)?.handle ?? id }))
    .sort((a, b) => a.label.localeCompare(b.label));
}

/** The frame around task groups: one card, groups divided by rules. */
export function TaskBoard({ children }: { children: ReactNode }) {
  return <div className="grid gap-4">{children}</div>;
}

/** A group of tasks under a shaded heading with its count. */
export function TaskGroupSection({
  id,
  heading,
  count,
  level = 2,
  children,
}: {
  id: string;
  heading: ReactNode;
  count: number;
  level?: 2 | 3;
  children: ReactNode;
}) {
  const Heading = level === 2 ? "h2" : "h3";
  return (
    <section className="overflow-hidden rounded-md border bg-card" aria-labelledby={id}>
      <Heading
        id={id}
        className="flex h-10 items-center gap-2 border-b bg-muted px-4 text-sm font-semibold"
      >
        {heading}
        <span className="h-5 min-w-5 rounded-full bg-secondary px-1.5 text-center text-xs leading-5 font-medium text-foreground tabular-nums">
          {count}
        </span>
      </Heading>
      {children}
    </section>
  );
}

interface TaskRowData {
  task: Task;
  agents: Map<string, Agent>;
  /** Ids of done tasks, so finished blockers drop out; unset where done tasks are not loaded. */
  done: ReadonlySet<string> | undefined;
  now: number;
}

const features = tableFeatures({});
const column = createColumnHelper<typeof features, TaskRowData>();

/** One task per row: priority, status, id, title with its tags, owner and last update. */
const columns = column.columns([
  column.display({
    id: "priority",
    cell: ({ row }) => <PriorityBadge priority={row.original.task.priority} />,
  }),
  column.display({
    id: "status",
    cell: ({ row }) => {
      const { task } = row.original;
      return (
        <StatusIcon status={task.status === "open" && task.ready ? "ready" : task.status} label />
      );
    },
  }),
  column.display({
    id: "id",
    cell: ({ row }) => row.original.task.id,
  }),
  column.display({ id: "main", cell: ({ row }) => <TaskMain {...row.original} /> }),
  column.display({
    id: "owner",
    cell: ({ row }) => <Owner task={row.original.task} agents={row.original.agents} />,
  }),
  column.display({
    id: "updated",
    cell: ({ row }) => <RelTime at={row.original.task.updated_at} />,
  }),
]);

// Desktop rows share the list's columns (subgrid), so the id column fits the longest id.
// On a phone a row stacks: status and title, then id, priority, owner and update.
const CELL_CLASS: Record<string, string> = {
  priority: "max-md:[grid-area:prio] max-md:self-center",
  status: "max-md:[grid-area:status] max-md:mt-0.75",
  id: "ident text-subtle-foreground text-xs max-md:[grid-area:id] max-md:self-center",
  main: "flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 max-md:[grid-area:main]",
  owner:
    "min-w-0 overflow-hidden max-md:[grid-area:owner] max-md:text-meta max-md:flex max-md:flex-wrap max-md:items-center max-md:[&_[data-slot=avatar]]:hidden",
  updated:
    "text-meta text-muted-foreground text-right whitespace-nowrap tabular-nums max-md:[grid-area:updated]",
};

export function TaskList({
  tasks,
  agents,
  done,
  now,
}: {
  tasks: Task[];
  agents: Map<string, Agent>;
  done?: ReadonlySet<string>;
  now: number;
}) {
  const data = useMemo(
    () => tasks.map((task) => ({ task, agents, done, now })),
    [tasks, agents, done, now],
  );
  const table = useTable({ features, columns, data, getRowId: (row) => row.task.id });
  return (
    <ul className="grid grid-cols-[54px_14px_minmax(84px,max-content)_minmax(0,1fr)_132px_88px] gap-x-3 divide-y max-md:block">
      {table.getRowModel().rows.map((row) => (
        <li
          key={row.id}
          className="relative col-span-full grid min-h-11 grid-cols-subgrid items-center gap-x-3 px-4 py-2 hover:bg-muted max-md:grid-cols-[14px_auto_auto_minmax(0,1fr)_auto] max-md:items-start max-md:gap-x-2.5 max-md:gap-y-1.5 max-md:px-3.5 max-md:py-3 max-md:[grid-template-areas:'status_main_main_main_main'_'._id_prio_owner_updated']"
        >
          {row.getAllCells().map((cell) => (
            <div key={cell.id} className={CELL_CLASS[cell.column.id]}>
              <table.FlexRender cell={cell} />
            </div>
          ))}
        </li>
      ))}
    </ul>
  );
}

function TaskMain({ task, done, now }: TaskRowData) {
  // Blockers already done no longer hold the task up.
  const blockers = done ? task.blockers.filter((id) => !done.has(id)) : [];
  // Only a date still ahead holds an open task up.
  const waitsUntil =
    task.status === "open" && task.not_before !== null && Date.parse(task.not_before) > now
      ? task.not_before
      : null;
  return (
    <>
      {/* The whole row opens the task; tags and blocker links sit above the link's cover. */}
      <Link
        className="min-w-0 py-0.5 font-semibold text-foreground after:absolute after:inset-0 hover:text-link"
        to="/tasks/$id"
        params={{ id: task.id }}
      >
        <PlainText className="whitespace-normal" text={task.title} />
      </Link>
      {blockers.length > 0 && (
        <Badge variant="outline" shape="tag" className="relative z-1">
          blocked by{" "}
          {blockers.map((id, index) => (
            <Fragment key={id}>
              {index > 0 && ", "}
              <Link
                className="inline-flex min-h-6 items-center tabular-nums"
                to="/tasks/$id"
                params={{ id }}
              >
                {id}
              </Link>
            </Fragment>
          ))}
        </Badge>
      )}
      {/* Most tasks may start on autopilot; mark the ones that wait for the user. */}
      {!task.auto && (
        <Badge
          variant="outline"
          shape="tag"
          className="relative z-1"
          title="Autopilot leaves this task for you to start"
          data-tag="manual"
        >
          manual
        </Badge>
      )}
      {waitsUntil && (
        <Badge variant="outline" shape="tag" className="relative z-1">
          waits until <AbsTime at={waitsUntil} />
        </Badge>
      )}
      {task.blocked_reason && (
        <p className="basis-full text-meta text-status-blocked">
          <span className="font-semibold">Blocked:</span> <PlainText text={task.blocked_reason} />
        </p>
      )}
    </>
  );
}
