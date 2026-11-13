import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { createColumnHelper, tableFeatures, useTable } from "@tanstack/react-table";
import { useMemo, useState } from "react";
import { api } from "@/api/client.ts";
import type { Agent, Board, Task } from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Switch } from "@/components/ui/switch.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { useNow } from "@/live/runtime.ts";
import {
  AgentStateBadge,
  agentIndex,
  Empty,
  ErrorNotice,
  ExpandableLine,
  PageHeader,
  QueryFrame,
  StatePill,
  useAgents,
  useTitle,
  Who,
} from "@/ui/common.tsx";
import { ConfirmDialog } from "@/ui/confirm.tsx";
import { Icon, StatusIcon, statusLabel } from "@/ui/icons.tsx";
import { ArrangedSections, Section } from "@/ui/sections.tsx";
import { boardTally, StatusTally } from "@/ui/tally.tsx";
import { AskRows, useOpenAsks } from "./asks.tsx";
import { TaskBoard, TaskGroupSection, TaskList } from "./board.tsx";

// Boards come second, so the overview is in reach without scrolling past the work in hand.
const HOME_SECTIONS = ["asks", "boards", "progress"] as const;

/**
 * Home: what waits on the user, every board and what is in progress. Each section collapses
 * and moves; the arrangement is kept per browser.
 */
export function HomeScreen() {
  useTitle("Home");
  const boards = useQuery(queries.boards());
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);
  const openAsks = useOpenAsks().data ?? [];
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
                  title="Waiting on you"
                  count={openAsks.length}
                  aside={
                    <Link
                      className="inline-flex min-h-7 items-center gap-1 text-meta font-medium max-md:min-h-11"
                      to="/asks"
                    >
                      All asks <Icon name="arrow" className="size-3.5" />
                    </Link>
                  }
                  {...state}
                >
                  <AskRows
                    asks={openAsks}
                    limit={HOME_ROWS}
                    empty="Nothing waits on you."
                    showBoard
                  />
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

interface BoardRowData {
  board: Board;
  coordinator: Agent | undefined;
  agents: Map<string, Agent>;
}

const features = tableFeatures({});
const column = createColumnHelper<typeof features, BoardRowData>();

/**
 * The boards table: name and goal, tasks by status, coordinator, open asks and autopilot.
 * At 760px of width and up the cells line up in columns under a header; below, a row
 * stacks name, tasks, coordinator, then asks and autopilot.
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
    cell: ({ row }) => (
      <StatusTally
        counts={boardTally(row.original.board.counts)}
        label="Tasks by status"
        readyAuto={row.original.board.counts.ready_auto}
        className="contents"
      />
    ),
  }),
  column.display({
    id: "coordinator",
    header: "Coordinator",
    cell: ({ row }) => <Coordinator {...row.original} />,
  }),
  column.display({
    id: "asks",
    header: "Asks",
    cell: ({ row }) => <BoardAsks board={row.original.board} />,
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
  asks: "[grid-area:asks]",
  // Not "auto": that is a keyword for grid-area and would place the cell automatically.
  autopilot: "[grid-area:pilot]",
};

const CELL_CLASS: Record<string, string> = {
  name: "grid min-w-0 gap-0.5",
  tasks: "grid min-w-0 gap-2 @min-[760px]:gap-1.75",
  coordinator: "text-meta flex min-w-0 flex-wrap items-center gap-x-2.5 gap-y-1",
  asks: "",
  autopilot: "flex flex-col items-end @min-[760px]:items-start",
};

const ROW_GRID =
  "grid grid-cols-[minmax(0,1fr)_auto] [grid-template-areas:'name_name'_'tasks_tasks'_'coord_coord'_'asks_pilot'] gap-x-3.5 gap-y-3 @min-[760px]:grid-cols-[minmax(0,1fr)_minmax(0,400px)_minmax(0,172px)_36px_64px] @min-[760px]:[grid-template-areas:'name_tasks_coord_asks_pilot'] @min-[760px]:gap-x-4";

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
              "items-center px-(--gutter) py-3 @min-[760px]:min-h-14 @min-[760px]:py-2 @min-[760px]:hover:bg-muted",
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
      {/* The name and slug are never cut: where the badges do not fit beside them, they
          move to a line of their own. */}
      <div className="flex min-w-0 flex-wrap items-center gap-x-2.5 gap-y-1">
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
          <Badge variant="outline" shape="tag">
            Temporary
          </Badge>
        )}
      </div>
      {board.goal ? (
        <ExpandableLine className="text-meta text-muted-foreground" text={board.goal} lines={2} />
      ) : (
        <p className="text-meta leading-6 text-muted-foreground">No goal written.</p>
      )}
    </>
  );
}

function Coordinator({ board, coordinator, agents }: BoardRowData) {
  if (board.coordinator === null) {
    return <span className="text-muted-foreground">No coordinator</span>;
  }
  if (!coordinator) return <Who id={board.coordinator} agents={agents} />;
  return (
    <>
      <Who id={coordinator.id} agents={agents} />
      <AgentStateBadge agent={coordinator} />
    </>
  );
}

function BoardAsks({ board }: { board: Board }) {
  if (board.open_asks === 0) {
    return (
      <span className="hidden text-meta text-muted-foreground @min-[760px]:inline">
        <span aria-hidden="true">–</span>
        <span className="sr-only">No asks</span>
      </span>
    );
  }
  return (
    <Link
      className="inline-flex min-h-7 items-center gap-1.25 text-meta font-semibold whitespace-nowrap text-amber tabular-nums"
      to="/asks"
      search={{ board: board.slug }}
    >
      <Icon name="inbox" className="size-3.5" />
      {board.open_asks}{" "}
      <span className="@min-[760px]:hidden">{board.open_asks === 1 ? "ask" : "asks"}</span>
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
            The daemon on one of the board's home devices works through ready tasks marked auto, a
            worker thread per task, without waiting for you. An agent sends an ask when it needs a
            decision.
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
