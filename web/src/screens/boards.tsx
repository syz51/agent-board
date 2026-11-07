import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { createColumnHelper, tableFeatures, useTable } from "@tanstack/react-table";
import { useMemo, useState } from "react";
import { api } from "@/api/client.ts";
import type { Agent, Board } from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Switch } from "@/components/ui/switch.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import {
  AgentStateBadge,
  agentIndex,
  AskKind,
  Empty,
  ErrorNotice,
  ExpandableLine,
  listClass,
  PageHeader,
  QueryFrame,
  RelTime,
  SectionTitle,
  StatePill,
  useAgents,
  useTitle,
  Who,
} from "@/ui/common.tsx";
import { ConfirmDialog } from "@/ui/confirm.tsx";
import { Icon, StatusIcon } from "@/ui/icons.tsx";
import { newestFirst, useOpenAsks } from "./asks.tsx";

export function BoardsScreen() {
  useTitle("Boards");
  const boards = useQuery(queries.boards());
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);
  const openAsks = useOpenAsks().data ?? [];

  return (
    <>
      <PageHeader
        title="Boards"
        sub={
          boards.data && (
            <>
              {boards.data.length} board{boards.data.length === 1 ? "" : "s"}
              {openAsks.length > 0 && <> · {openAsks.length} waiting on you</>}
            </>
          )
        }
      />
      {openAsks.length > 0 && (
        <section className="mb-8" aria-labelledby="waiting-title">
          <div className="flex items-baseline justify-between gap-3">
            <SectionTitle id="waiting-title">Waiting on you</SectionTitle>
            <Link
              className="inline-flex min-h-7 items-center gap-1 text-meta font-medium"
              to="/asks"
            >
              All asks <Icon name="arrow" className="size-3.5" />
            </Link>
          </div>
          <ul className={listClass}>
            {newestFirst(openAsks)
              .slice(0, 4)
              .map((ask) => (
                <li key={ask.id}>
                  <Link
                    className="grid min-h-11 grid-cols-[96px_minmax(0,1fr)_auto] items-center gap-3.5 px-4 py-2 text-foreground hover:bg-muted hover:no-underline max-md:grid-cols-[minmax(0,1fr)] max-md:gap-1"
                    to="/asks/$id"
                    params={{ id: String(ask.id) }}
                  >
                    <AskKind kind={ask.kind} />
                    <span
                      className="truncate font-medium max-md:line-clamp-2 max-md:whitespace-normal"
                      title={plainLine(ask.question)}
                    >
                      {plainLine(ask.question)}
                    </span>
                    <span className="text-meta whitespace-nowrap text-muted-foreground">
                      {ask.board} · <RelTime at={ask.created_at} />
                    </span>
                  </Link>
                </li>
              ))}
          </ul>
        </section>
      )}
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
    </>
  );
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
  { key: "done", label: "done", bar: "bg-status-done" },
  { key: "review", label: "review", bar: "bg-status-review" },
  { key: "doing", label: "doing", bar: "bg-status-doing" },
  { key: "blocked", label: "blocked", bar: "bg-status-blocked" },
  { key: "ready", label: "ready", bar: "bg-status-ready" },
  { key: "waiting", label: "waiting", bar: "bg-input" },
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
 * The boards table: name and goal, tasks by status, coordinator, open asks and autopilot.
 * At 760px of width and up the cells line up in columns under a header; below, a row
 * stacks name, tasks, then coordinator, asks and autopilot.
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
  autopilot: "[grid-area:auto]",
};

const CELL_CLASS: Record<string, string> = {
  name: "grid min-w-0 gap-1",
  tasks: "grid min-w-0",
  coordinator: "text-meta flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1",
  asks: "",
  autopilot: "flex flex-col items-end @min-[760px]:self-start",
};

/** A repository-list row: name and goal with autopilot at the right, then one line of facts. */
const ROW_GRID =
  "grid grid-cols-[minmax(0,1fr)_auto_auto] [grid-template-areas:'name_name_name'_'tasks_tasks_tasks'_'coord_asks_auto'] gap-x-3.5 gap-y-3 @min-[760px]:grid-cols-[auto_auto_auto_minmax(0,1fr)_auto] @min-[760px]:[grid-template-areas:'name_name_name_name_auto'_'tasks_coord_asks_._.'] @min-[760px]:gap-x-6 @min-[760px]:gap-y-2.5";

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
    <section
      className="@container overflow-hidden rounded-md border bg-card"
      aria-label="All boards"
    >
      {table.getHeaderGroups().map((group) => (
        <div key={group.id} className={cn(ROW_GRID, "hidden")} aria-hidden="true">
          {group.headers.map((header) => (
            <span key={header.id} className={AREA[header.id]}>
              <table.FlexRender header={header} />
            </span>
          ))}
        </div>
      ))}
      <ul className="divide-y">
        {table.getRowModel().rows.map((row) => (
          <li key={row.id} className={cn(ROW_GRID, "items-center px-4 py-4 @min-[760px]:py-5")}>
            {row.getAllCells().map((cell) => (
              <div key={cell.id} className={cn(AREA[cell.column.id], CELL_CLASS[cell.column.id])}>
                <table.FlexRender cell={cell} />
              </div>
            ))}
          </li>
        ))}
      </ul>
    </section>
  );
}

function BoardName({ board }: { board: Board }) {
  return (
    <>
      <div className="flex min-w-0 flex-wrap items-center gap-x-2.5 gap-y-1">
        <h2 className="min-w-0 text-[20px] leading-7 font-semibold">
          <Link className="text-link" to="/boards/$slug" params={{ slug: board.slug }}>
            {board.title}
          </Link>
        </h2>
        <span className="ident h-5 rounded-full border px-2 text-xs leading-[18px] text-muted-foreground">
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
        <ExpandableLine className="text-sm text-muted-foreground" text={board.goal} />
      ) : (
        <p className="text-sm leading-6 text-muted-foreground">No goal written.</p>
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
        className="flex h-1.5 gap-0.5 overflow-hidden rounded-[3px] bg-secondary @min-[760px]:hidden"
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
          className="flex flex-wrap gap-x-3 gap-y-1 text-meta leading-[18px] text-muted-foreground max-[759px]:mt-2"
          aria-label="Tasks by status"
        >
          {SEGMENTS.map(
            (segment) =>
              counts[segment.key] > 0 && (
                <li
                  key={segment.key}
                  className="inline-flex items-center gap-1"
                  title={`${counts[segment.key]} ${segment.label}`}
                >
                  <StatusIcon
                    status={segment.key === "waiting" ? "open" : segment.key}
                    className="size-3"
                  />
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
    return <span className="text-muted-foreground">No coordinator</span>;
  }
  if (!coordinator) return <Who id={board.coordinator} agents={agents} />;
  return (
    <>
      <Who id={coordinator.id} agents={agents} />
      <span className="hidden @min-[760px]:inline-flex">
        <AgentStateBadge agent={coordinator} />
      </span>
    </>
  );
}

function BoardAsks({ board }: { board: Board }) {
  if (board.open_asks === 0) {
    return (
      <span className="hidden text-meta text-muted-foreground @min-[760px]:inline">No asks</span>
    );
  }
  return (
    <Link
      className="inline-flex min-h-7 items-center gap-1.25 text-meta font-semibold whitespace-nowrap text-amber tabular-nums"
      to="/asks"
      search={{ board: board.slug }}
    >
      <Icon name="inbox" className="size-3.5" />
      {board.open_asks} <span>{board.open_asks === 1 ? "ask" : "asks"}</span>
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
        <span>Autopilot</span>
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
