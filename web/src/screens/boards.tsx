import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { createColumnHelper, tableFeatures, useTable } from "@tanstack/react-table";
import { useMemo, useState } from "react";
import { api } from "@/api/client.ts";
import type { Agent, Ask, Board, Task } from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Switch } from "@/components/ui/switch.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { PlainText } from "@/text/markdown.tsx";
import { useNow } from "@/live/runtime.ts";
import {
  AgentStateBadge,
  agentIndex,
  AskKind,
  Empty,
  ErrorNotice,
  ExpandableLine,
  PageHeader,
  QueryFrame,
  listClass,
  RelTime,
  StatePill,
  useAgents,
  useTitle,
  Who,
} from "@/ui/common.tsx";
import { ConfirmDialog } from "@/ui/confirm.tsx";
import { Icon, StatusIcon, statusLabel } from "@/ui/icons.tsx";
import { ArrangedSections, Section } from "@/ui/sections.tsx";
import { newestFirst, useOpenAsks } from "./asks.tsx";
import { TaskBoard, TaskGroupSection, TaskList } from "./board.tsx";
import { unseenCount, Updates, UpdatesHeader, useUpdates } from "./updates.tsx";

// Boards come second, so the overview is in reach without scrolling past the work in hand.
const HOME_SECTIONS = ["asks", "boards", "progress", "updates"] as const;

/**
 * Home: what waits on the user, every board, what is in progress and what changed since they
 * last looked. Each section collapses and moves; the arrangement is kept per browser.
 */
export function HomeScreen() {
  useTitle("Home");
  const boards = useQuery(queries.boards());
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);
  const openAsks = useOpenAsks().data ?? [];
  const updates = useUpdates().data;
  const active = useQuery(queries.activeTasks());
  const closed = useMemo(
    () =>
      new Set(
        (boards.data ?? []).filter((board) => board.status === "closed").map((board) => board.slug),
      ),
    [boards.data],
  );
  const now = useNow(60_000);

  return (
    <>
      <PageHeader
        title="Home"
        sub={
          boards.data && (
            <>
              {boards.data.length} board{boards.data.length === 1 ? "" : "s"}
              {openAsks.length > 0 && <> · {openAsks.length} waiting on you</>}
            </>
          )
        }
      />
      <ArrangedSections
        storageKey="home"
        ids={HOME_SECTIONS}
        render={(id, state) => {
          switch (id) {
            case "asks":
              return (
                <Section
                  title="Needs you"
                  count={openAsks.length}
                  aside={
                    <Link
                      className="inline-flex min-h-7 items-center gap-1 text-meta font-medium max-md:min-h-11"
                      to="/asks"
                    >
                      See all <Icon name="arrow" className="size-3.5" />
                    </Link>
                  }
                  {...state}
                >
                  <WaitingAsks asks={openAsks} />
                </Section>
              );
            case "progress":
              return (
                <Section
                  title="In progress"
                  count={active.data && inProgress(active.data, closed).length}
                  {...state}
                >
                  <QueryFrame query={active} label="Loading tasks">
                    {(list) => (
                      <InProgress tasks={inProgress(list, closed)} agents={agents} now={now} />
                    )}
                  </QueryFrame>
                </Section>
              );
            case "updates":
              return (
                <Section
                  title="Since you last looked"
                  count={updates ? unseenCount(updates) : undefined}
                  aside={updates && <UpdatesHeader updates={updates} />}
                  {...state}
                >
                  <Updates />
                </Section>
              );
            case "boards":
              return (
                <Section title="Boards" count={boards.data?.length} {...state}>
                  <QueryFrame query={boards} label="Loading boards">
                    {(list) =>
                      list.length === 0 ? (
                        <Empty title="No boards yet">
                          Create one with <code>board create</code>; it appears here.
                        </Empty>
                      ) : (
                        <BoardTable boards={list} agents={agents} />
                      )
                    }
                  </QueryFrame>
                </Section>
              );
          }
        }}
      />
    </>
  );
}

/** How many open asks Home lists; Needs you lists them all. */
export const HOME_ROWS = 4;

/**
 * Tasks in progress, under Doing and Review headings as on a board, so the two read apart at
 * a glance.
 */
function InProgress({
  tasks,
  agents,
  now,
}: {
  tasks: Task[];
  agents: Map<string, Agent>;
  now: number;
}) {
  if (tasks.length === 0) {
    return <p className="text-meta text-muted-foreground">No task is in progress.</p>;
  }
  const groups = (["doing", "review"] as const)
    .map((status) => ({ status, tasks: tasks.filter((task) => task.status === status) }))
    .filter((group) => group.tasks.length > 0);
  return (
    <TaskBoard>
      {groups.map((group) => (
        <TaskGroupSection
          key={group.status}
          id={`progress-${group.status}`}
          level={3}
          heading={
            <>
              <StatusIcon status={group.status} />
              {statusLabel(group.status)}
            </>
          }
          count={group.tasks.length}
        >
          <TaskList tasks={group.tasks} agents={agents} now={now} />
        </TaskGroupSection>
      ))}
    </TaskBoard>
  );
}

/** The newest open asks, one line each; the Needs you page lists them all. */
function WaitingAsks({ asks }: { asks: Ask[] }) {
  if (asks.length === 0) {
    return <p className="text-meta text-muted-foreground">Nothing waits on you.</p>;
  }
  return (
    <ul className={listClass}>
      {newestFirst(asks)
        .slice(0, HOME_ROWS)
        .map((ask) => (
          <li key={ask.id}>
            {/* The left edge takes the kind's colour, as on the Needs you cards: these rows
                wait on the user, unlike every other row on Home. */}
            <Link
              className={cn(
                "group/ask relative grid min-h-9 grid-cols-[96px_minmax(0,1fr)_auto] items-center gap-3.5 py-1.5 pr-(--gutter) pl-[calc(var(--gutter)+12px)] text-foreground hover:bg-muted hover:no-underline max-md:grid-cols-[auto_minmax(0,1fr)] max-md:gap-x-2.5 max-md:gap-y-0.5",
                "before:absolute before:inset-y-2 before:left-(--gutter) before:w-[3px] before:rounded-full",
                ask.kind === "action" ? "before:bg-amber" : "before:bg-status-review",
              )}
              to="/asks/$id"
              params={{ id: String(ask.id) }}
            >
              <AskKind kind={ask.kind} />
              <span
                className="truncate font-medium max-md:col-span-2 max-md:row-start-2 max-md:line-clamp-3 max-md:whitespace-normal"
                title={plainLine(ask.question)}
              >
                {/* Tags show as code here too, as in the ask itself. */}
                <PlainText className="[white-space:inherit]" text={plainLine(ask.question)} />
              </span>
              {/* Where and when; on hover or focus, what a click does. */}
              <span className="grid text-meta whitespace-nowrap text-muted-foreground max-md:col-start-2 max-md:row-start-1">
                <span className="col-start-1 row-start-1 [@media(hover:hover)]:group-hover/ask:invisible [@media(hover:hover)]:group-focus-visible/ask:invisible">
                  {ask.board} · <RelTime at={ask.created_at} />
                </span>
                <span
                  className="invisible col-start-1 row-start-1 inline-flex items-center justify-end gap-1 font-medium text-link [@media(hover:hover)]:group-hover/ask:visible [@media(hover:hover)]:group-focus-visible/ask:visible"
                  aria-hidden="true"
                >
                  Answer <Icon name="arrow" className="size-3.5" />
                </span>
              </span>
            </Link>
          </li>
        ))}
    </ul>
  );
}

/**
 * Tasks being worked on or waiting for review, on boards that are not closed: doing first,
 * then review, each most recently updated first. Exported for tests.
 */
export function inProgress(tasks: Task[], closedBoards: ReadonlySet<string>): Task[] {
  const rank = (task: Task) => (task.status === "doing" ? 0 : 1);
  return tasks
    .filter(
      (task) =>
        (task.status === "doing" || task.status === "review") && !closedBoards.has(task.board),
    )
    .sort((a, b) => rank(a) - rank(b) || Date.parse(b.updated_at) - Date.parse(a.updated_at));
}

/** The first line of agent markdown without its markup, for a one-line preview. */
export function plainLine(text: string): string {
  const line = text.trim().split("\n")[0] ?? "";
  return line
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/(\*\*|__|`|~~)/g, "")
    .replace(/^#+\s*/, "");
}

const SEGMENTS = [
  { key: "done", label: "done", bar: "bg-status-done", icon: "done" },
  { key: "review", label: "review", bar: "bg-status-review", icon: "review" },
  { key: "doing", label: "doing", bar: "bg-status-doing", icon: "doing" },
  { key: "blocked", label: "blocked", bar: "bg-status-blocked", icon: "blocked" },
  { key: "ready", label: "ready", bar: "bg-status-ready", icon: "ready" },
  // Not started: the track's grey in the bar, and the dashed ring of an open task.
  { key: "waiting", label: "waiting", bar: "bg-input", icon: "open" },
] as const;

function segmentCounts(counts: Board["counts"]): Record<(typeof SEGMENTS)[number]["key"], number> {
  return {
    done: counts.done,
    review: counts.review,
    doing: counts.doing,
    blocked: counts.blocked,
    ready: counts.ready,
    waiting: Math.max(0, counts.open - counts.ready),
  };
}

interface BoardRowData {
  board: Board;
  coordinator: Agent | undefined;
  agents: Map<string, Agent>;
}

const features = tableFeatures({});
const column = createColumnHelper<typeof features, BoardRowData>();

/**
 * The boards table: name and goal (with any open asks beside the name), tasks by status,
 * coordinator and autopilot. At 760px of width and up the cells line up in columns under
 * a header; below, a row stacks name, tasks, then coordinator and autopilot.
 */
const columns = column.columns([
  column.display({
    id: "name",
    header: "Board",
    cell: ({ row }) => <BoardName board={row.original.board} />,
  }),
  column.display({
    id: "tasks",
    header: "Tasks",
    cell: ({ row }) => <TaskTally board={row.original.board} />,
  }),
  column.display({
    id: "coordinator",
    header: "Coordinator",
    cell: ({ row }) => <Coordinator {...row.original} />,
  }),
  column.display({
    id: "autopilot",
    header: "Autopilot",
    cell: ({ row }) => <AutopilotSwitch board={row.original.board} />,
  }),
]);

const AREA: Record<string, string> = {
  name: "[grid-area:name]",
  tasks: "[grid-area:tasks]",
  coordinator: "[grid-area:coord]",
  // Not "auto": that is a keyword for grid-area and would place the cell automatically.
  autopilot: "[grid-area:pilot]",
};

const CELL_CLASS: Record<string, string> = {
  name: "grid min-w-0 gap-0.5",
  tasks: "grid min-w-0 gap-2 @min-[760px]:gap-1.75",
  coordinator: "text-meta flex min-h-5 min-w-0 flex-wrap items-center gap-x-2.5 gap-y-1",
  autopilot: "flex flex-col items-end @min-[760px]:items-start",
};

/**
 * The boards table's columns. Every row is one height on wide screens: the name line, with
 * its whole slug, states and open asks, the goal in one line, the tally in one and the
 * coordinator in two at most. Cells start at the top of the row, level with the name.
 */
const ROW_GRID =
  "grid grid-cols-[minmax(0,1fr)_auto] [grid-template-areas:'name_name'_'tasks_tasks'_'coord_pilot'] gap-x-3.5 gap-y-3 @min-[760px]:grid-cols-[minmax(0,1fr)_minmax(0,416px)_minmax(0,160px)_68px] @min-[760px]:[grid-template-areas:'name_tasks_coord_pilot'] @min-[760px]:gap-x-4";

function BoardTable({ boards, agents }: { boards: Board[]; agents: Map<string, Agent> }) {
  const data = useMemo(
    () =>
      boards.map((board) => ({
        board,
        coordinator: board.coordinator ? agents.get(board.coordinator) : undefined,
        agents,
      })),
    [boards, agents],
  );
  const table = useTable({
    features,
    columns,
    data,
    getRowId: (row) => row.board.slug,
  });
  return (
    <div className="edge @container overflow-hidden border-y bg-card">
      {table.getHeaderGroups().map((group) => (
        <div
          key={group.id}
          className={cn(
            ROW_GRID,
            "hidden h-8 items-center border-b bg-muted px-(--gutter) text-xs font-medium text-muted-foreground @min-[760px]:grid",
          )}
          aria-hidden="true"
        >
          {group.headers.map((header) => (
            <span key={header.id} className={AREA[header.id]}>
              <table.FlexRender header={header} />
            </span>
          ))}
        </div>
      ))}
      <ul className="divide-y">
        {table.getRowModel().rows.map((row) => (
          <li
            key={row.id}
            className={cn(
              ROW_GRID,
              "items-center px-(--gutter) py-3 @min-[760px]:min-h-17 @min-[760px]:items-start @min-[760px]:py-2.5 @min-[760px]:hover:bg-muted",
            )}
          >
            {row.getAllCells().map((cell) => (
              <div key={cell.id} className={cn(AREA[cell.column.id], CELL_CLASS[cell.column.id])}>
                <table.FlexRender cell={cell} />
              </div>
            ))}
          </li>
        ))}
      </ul>
    </div>
  );
}

function BoardName({ board }: { board: Board }) {
  return (
    <>
      {/* One line: the name, its slug and its states, none of them cut. */}
      <div className="flex min-h-5 min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
        <h3 className="min-w-0 text-sm font-semibold tracking-[-0.006em] [overflow-wrap:anywhere]">
          <Link
            className="text-foreground"
            to="/boards/$slug"
            params={{ slug: board.slug }}
            title={board.title}
          >
            {board.title}
          </Link>
        </h3>
        <span className="ident min-w-0 text-meta [overflow-wrap:anywhere] text-muted-foreground">
          {board.slug}
        </span>
        {board.status !== "active" && (
          <StatePill state={board.status}>
            {board.status[0]?.toUpperCase()}
            {board.status.slice(1)}
          </StatePill>
        )}
        {board.temporary && (
          <Badge variant="outline" shape="tag" className="flex-none">
            Temporary
          </Badge>
        )}
        <BoardAsks board={board} />
      </div>
      {board.goal ? (
        <ExpandableLine className="text-meta text-muted-foreground" text={board.goal} lines={1} />
      ) : (
        <p className="text-meta leading-6 text-muted-foreground">No goal written.</p>
      )}
    </>
  );
}

function TaskTally({ board }: { board: Board }) {
  const counts = segmentCounts(board.counts);
  const total = SEGMENTS.reduce((sum, segment) => sum + counts[segment.key], 0);
  return (
    <>
      <div
        className="flex h-1.5 gap-0.5 overflow-hidden rounded-[3px] bg-secondary"
        aria-hidden="true"
      >
        {SEGMENTS.map(
          (segment) =>
            counts[segment.key] > 0 && (
              <span
                key={segment.key}
                className={cn("min-w-1 basis-0", segment.bar)}
                style={{ flexGrow: counts[segment.key] }}
              />
            ),
        )}
      </div>
      {total === 0 ? (
        <p className="text-meta leading-[18px] text-muted-foreground">No tasks yet</p>
      ) : (
        <ul
          // Phones set it a step smaller and tighter, so six counts still fit on one line.
          className="grid grid-cols-3 gap-x-2 gap-y-0.5 text-meta leading-[18px] text-muted-foreground @min-[760px]:flex @min-[760px]:flex-nowrap @min-[760px]:gap-x-1.5"
          aria-label="Tasks by status"
        >
          {SEGMENTS.map(
            (segment) =>
              counts[segment.key] > 0 && (
                // A count and its word, in the bar's order, after a dot of its bar colour.
                <li
                  key={segment.key}
                  className="inline-flex items-center gap-0.75 whitespace-nowrap"
                >
                  <StatusIcon status={segment.icon} className="size-3 @min-[760px]:size-2.5" />
                  <strong className="font-semibold text-foreground tabular-nums">
                    {counts[segment.key]}
                  </strong>{" "}
                  <span>{segment.label}</span>
                </li>
              ),
          )}
        </ul>
      )}
    </>
  );
}

function Coordinator({ board, coordinator, agents }: BoardRowData) {
  if (board.coordinator === null) {
    return (
      <span className="text-muted-foreground">
        <span className="@min-[760px]:hidden">No coordinator</span>
        <span className="hidden @min-[760px]:inline">None</span>
      </span>
    );
  }
  if (!coordinator) return <Who id={board.coordinator} agents={agents} />;
  return (
    <>
      <Who id={coordinator.id} agents={agents} />
      <AgentStateBadge agent={coordinator} />
    </>
  );
}

/** A board's open asks, beside its name only when there are some, in the sidebar's tint. */
function BoardAsks({ board }: { board: Board }) {
  if (board.open_asks === 0) return null;
  return (
    // A 24px target that keeps the name line's 20px height.
    <Link
      className="-my-0.5 inline-flex min-h-6 items-center hover:no-underline"
      to="/asks"
      search={{ board: board.slug }}
    >
      <span className="inline-flex h-5 items-center gap-1 rounded-full bg-amber-soft px-2 text-xs font-semibold whitespace-nowrap text-amber tabular-nums ring-1 ring-amber/30 ring-inset">
        <Icon name="inbox" className="size-3" />
        {board.open_asks} {board.open_asks === 1 ? "ask" : "asks"}
      </span>
    </Link>
  );
}

function AutopilotSwitch({ board }: { board: Board }) {
  const client = useQueryClient();
  const [confirming, setConfirming] = useState(false);
  const turningOn = !board.autopilot;
  const mutation = useMutation({
    mutationFn: fetching((on: boolean) => api.setAutopilot(board.slug, on)),
    onSuccess: () => invalidateTags(client, [tags.boards, tags.board(board.slug)]),
  });

  return (
    <>
      <Switch
        checked={board.autopilot}
        aria-busy={mutation.isPending}
        disabled={mutation.isPending}
        // The dialog confirms the change; the switch itself only opens it.
        onCheckedChange={() => setConfirming(true)}
      >
        <span className="@min-[760px]:sr-only">Autopilot</span>
      </Switch>
      <ConfirmDialog
        open={confirming}
        title={`Turn autopilot ${turningOn ? "on" : "off"} for ${board.title}?`}
        confirmLabel={turningOn ? "Turn on" : "Turn off"}
        onConfirm={() => {
          setConfirming(false);
          mutation.mutate(turningOn);
        }}
        onCancel={() => setConfirming(false)}
      >
        {turningOn ? (
          <p>
            A coordinator thread starts on one of the board's home devices and works through ready
            tasks marked auto, one at a time, without waiting for you. It sends an ask when it needs
            a decision.
          </p>
        ) : (
          <p>
            No coordinator will pick up ready tasks; they wait for an agent or for you to start
            them.
          </p>
        )}
      </ConfirmDialog>
      {mutation.error && (
        <div className="mt-1.5">
          <ErrorNotice error={mutation.error} compact />
        </div>
      )}
    </>
  );
}
