import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { getRouteApi, Link } from "@tanstack/react-router";
import { createColumnHelper, tableFeatures, useTable } from "@tanstack/react-table";
import { Fragment, type ReactNode, useEffect, useId, useMemo, useState } from "react";
import { type ApiFailure, api, type UpdateTaskPatch } from "@/api/client.ts";
import type { Agent, Priority, Task } from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import { NativeSelectOption } from "@/components/ui/native-select.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { useNow } from "@/live/runtime.ts";
import type { BoardSearch, BoardTab } from "@/router.tsx";
import { PlainText } from "@/text/markdown.tsx";
import {
  agentIndex,
  BLOCKERS_CLOSED,
  Crumbs,
  DayTime,
  Empty,
  ErrorNotice,
  Owner,
  PageHeader,
  PriorityBadge,
  QueryFrame,
  RelTime,
  useAgents,
  useTitle,
} from "@/ui/common.tsx";
import { FilterSelect, FilterToggle, SearchField } from "@/ui/filters.tsx";
import { Icon, StatusIcon, statusKey } from "@/ui/icons.tsx";
import { useCollapsed } from "@/ui/sections.tsx";
import { StatusTally, taskTally } from "@/ui/tally.tsx";
import {
  buildPinTopOrder,
  buildReorderOrder,
  effectivePriorityOf,
  formatCadenceSecs,
  InlineDropForm,
  matchesTaskFilters,
  partitionBoardTiers,
  PriorityQuickPicker,
  TaskSignalBadges,
  taskNumber,
  UP_NEXT_FRONTIER_SIZE,
} from "@/ui/task-signals.tsx";
import { AskRows, useOpenAsks } from "./asks.tsx";

const route = getRouteApi("/boards/$slug");

const DONE_WINDOW_MS = 30 * 86_400_000;

export interface TaskFilters {
  q: string;
  priority: string;
  /** An agent id, `none` for unowned, or empty for anyone. */
  owner: string;
  readyOnly: boolean;
  /** A task id: only its direct children, done ones whenever they were done. */
  parent: string;
}

export interface TaskGroup {
  key: "ready" | "open" | "doing" | "blocked" | "review" | "done" | "dropped";
  title: string;
  tasks: Task[];
}

/**
 * The server's list order: effective priority, then ranked tasks by rank, then the rest by number.
 * Exported for tests.
 */
export function inListOrder(a: Task, b: Task): number {
  return (
    effectivePriorityOf(a).localeCompare(effectivePriorityOf(b)) ||
    Number(a.rank == null) - Number(b.rank == null) ||
    (a.rank ?? 0) - (b.rank ?? 0) ||
    taskNumber(a) - taskNumber(b)
  );
}

/**
 * Group a board's tasks for display: doing, review, blocked, ready, open tasks still
 * waiting, and done in the last 30 days. Dropped tasks are not shown unless narrowed to a
 * parent. The children of the `parent` filter's task show whole: every done one, then the
 * dropped ones, so the list holds as many as the parent's subtask count. Exported for tests.
 */
export function groupTasks(tasks: Task[], filters: TaskFilters, now: number): TaskGroup[] {
  const q = filters.q.trim().toLowerCase();
  const visible = tasks.filter((task) => {
    if (filters.parent ? task.parent !== filters.parent : task.status === "dropped") return false;
    if (!filters.parent && task.status === "open" && task.cooled_at !== null) return false;
    if (
      !filters.parent &&
      task.status === "done" &&
      now - Date.parse(task.updated_at) > DONE_WINDOW_MS
    ) {
      return false;
    }
    if (
      filters.priority &&
      task.priority !== filters.priority &&
      task.effective_priority !== filters.priority
    ) {
      return false;
    }
    if (
      filters.owner === "none" ? task.owner !== null : filters.owner && task.owner !== filters.owner
    ) {
      return false;
    }
    if (filters.readyOnly && !task.ready) return false;
    return !q || task.title.toLowerCase().includes(q) || task.id.toLowerCase().includes(q);
  });
  visible.sort(inListOrder);
  const of = (status: Task["status"]) => visible.filter((task) => task.status === status);
  const open = of("open");
  const done = of("done").sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
  return [
    { key: "doing", title: "Doing", tasks: of("doing") },
    { key: "review", title: "Review", tasks: of("review") },
    { key: "blocked", title: "Blocked", tasks: of("blocked") },
    { key: "ready", title: "Ready", tasks: open.filter((task) => task.ready) },
    { key: "open", title: "Waiting", tasks: open.filter((task) => !task.ready) },
    { key: "done", title: filters.parent ? "Done" : "Done in the last 30 days", tasks: done },
    { key: "dropped", title: "Dropped", tasks: of("dropped") },
  ];
}

const NO_FILTERS: TaskFilters = { q: "", priority: "", owner: "", readyOnly: false, parent: "" };

/**
 * Whether a board shows any task before filtering, by the rule the list uses: dropped tasks
 * and tasks done over 30 days ago do not count. Exported for tests.
 */
export function showsTasks(tasks: Task[], now: number): boolean {
  return groupTasks(tasks, NO_FILTERS, now).some((group) => group.tasks.length > 0);
}

/** The filters a board's URL holds. */
export function filtersOf(search: BoardSearch): TaskFilters {
  return {
    q: search.q ?? "",
    priority: search.priority ?? "",
    owner: search.owner ?? "",
    readyOnly: search.ready === "1",
    parent: search.parent ?? "",
  };
}

const TITLES_KEY = "board-titles";

/** Board names last seen in this browser, by slug; a convenience, so failures are ignored. */
function storedTitles(): Record<string, string> {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(TITLES_KEY) ?? "{}");
    return value && typeof value === "object" ? (value as Record<string, string>) : {};
  } catch {
    return {};
  }
}

function rememberedTitle(slug: string): string | undefined {
  const title = storedTitles()[slug];
  return typeof title === "string" ? title : undefined;
}

function rememberTitle(slug: string, title: string) {
  const titles = storedTitles();
  if (titles[slug] === title) return;
  try {
    localStorage.setItem(TITLES_KEY, JSON.stringify({ ...titles, [slug]: title }));
  } catch {
    // Storage may be full or blocked; the slug then stands in during an outage.
  }
}

interface UpdateVars {
  task: Task;
  patch: Omit<UpdateTaskPatch, "version">;
}

interface RankVars {
  taskIds: string[];
  priority: Priority;
}

export interface TaskRowActions {
  busyIds: ReadonlySet<string>;
  allowReorder?: boolean | undefined;
  onPinTop: (task: Task) => void;
  onPriorityChange: (task: Task, priority: Priority) => void;
  onCoolToggle: (task: Task, cooled: boolean) => void;
  onDrop: (task: Task, reason: string) => void;
  onReorder?:
    | ((draggedId: string, targetId: string, position: "before" | "after") => void)
    | undefined;
}

export function BoardScreen() {
  const { slug } = route.useParams();
  const search = route.useSearch();
  const filters = filtersOf(search);
  const activeTab: BoardTab = search.tab ?? "up-next";
  const navigate = route.useNavigate();
  const queryClient = useQueryClient();
  const now = useNow(60_000);
  // The board comes from the list the sidebar already holds: one request for both.
  const boards = useQuery(queries.boards());
  const board = boards.data?.find((item) => item.slug === slug);
  const boardTasks = useQuery(queries.boardTasks(slug));
  // Narrowed to a parent, the list asks the server for its children: the board list is
  // capped, so it may miss some.
  const childList = useQuery({
    ...queries.childTasks(slug, filters.parent),
    enabled: filters.parent !== "",
  });
  // Cooled Icebox tasks are fetched lazily when the Icebox tab is opened.
  const iceboxQuery = useQuery({
    ...queries.iceboxTasks(slug),
    enabled: !filters.parent && activeTab === "icebox",
  });
  const tasks = filters.parent ? childList : boardTasks;
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);
  // One set per task list, so the memoized rows keep their identity between renders.
  // Blockers come from anywhere on the board, so this set does too.
  const done = useMemo(
    () =>
      new Set(
        (boardTasks.data ?? []).filter((task) => task.status === "done").map((task) => task.id),
      ),
    [boardTasks.data],
  );
  // Without the board list, as in an outage, the name last seen in this browser stands in,
  // or else the slug.
  useEffect(() => {
    if (board) rememberTitle(slug, board.title);
  }, [slug, board]);
  const name = board?.title ?? rememberedTitle(slug) ?? slug;
  useTitle(name);
  // One choice for every board: done and dropped work starts folded away.
  const [collapsed, toggle] = useCollapsed("board-groups", ["done", "dropped"]);

  // Mutation feedback state: optimistic busy set + dismissible inline error notice.
  const [busyIds, setBusyIds] = useState<Set<string>>(() => new Set());
  const [actionError, setActionError] = useState<ApiFailure | null>(null);

  const markBusy = (ids: readonly string[], busy: boolean) =>
    setBusyIds((prev) => {
      const next = new Set(prev);
      for (const id of ids) {
        if (busy) next.add(id);
        else next.delete(id);
      }
      return next;
    });

  const boardKey = queries.boardTasks(slug).queryKey;
  const iceboxKey = queries.iceboxTasks(slug).queryKey;

  const updateMutation = useMutation<
    Task,
    ApiFailure,
    UpdateVars,
    { prevBoard?: Task[] | undefined; prevIcebox?: Task[] | undefined }
  >({
    mutationFn: fetching(({ task, patch }: UpdateVars) =>
      api.updateTask(task.id, { version: task.version, ...patch }),
    ),
    async onMutate({ task, patch }) {
      setActionError(null);
      markBusy([task.id], true);
      await queryClient.cancelQueries({ queryKey: boardKey });
      await queryClient.cancelQueries({ queryKey: iceboxKey });
      const prevBoard = queryClient.getQueryData<Task[]>(boardKey);
      const prevIcebox = queryClient.getQueryData<Task[]>(iceboxKey);
      const updatedAt = new Date().toISOString();

      const applyPatch = (item: Task): Task => {
        if (item.id !== task.id) return item;
        const nextPriority = patch.priority ?? item.priority;
        const priorityChanged = patch.priority !== undefined && patch.priority !== item.priority;
        const nextRank = patch.rank !== undefined ? patch.rank : priorityChanged ? null : item.rank;
        const nextCooled =
          patch.cooled === true ? updatedAt : patch.cooled === false ? null : item.cooled_at;
        const nextStatus = patch.status ?? item.status;
        return {
          ...item,
          priority: nextPriority,
          effective_priority: priorityChanged ? null : item.effective_priority,
          rank: nextRank,
          cooled_at: nextCooled,
          status: nextStatus,
          ready: nextStatus === "open" && nextCooled === null ? item.ready : false,
          updated_at: updatedAt,
        };
      };

      if (prevBoard) {
        if (patch.cooled === true) {
          queryClient.setQueryData<Task[]>(
            boardKey,
            prevBoard.filter((item) => item.id !== task.id),
          );
        } else if (patch.cooled === false) {
          const revived = applyPatch(task);
          const exists = prevBoard.some((item) => item.id === task.id);
          queryClient.setQueryData<Task[]>(
            boardKey,
            exists ? prevBoard.map(applyPatch) : [revived, ...prevBoard],
          );
        } else {
          queryClient.setQueryData<Task[]>(boardKey, prevBoard.map(applyPatch));
        }
      }
      if (prevIcebox) {
        if (patch.cooled === false || patch.status === "dropped") {
          queryClient.setQueryData<Task[]>(
            iceboxKey,
            prevIcebox.filter((item) => item.id !== task.id),
          );
        } else if (patch.cooled === true) {
          const cooledTask = applyPatch(task);
          const exists = prevIcebox.some((item) => item.id === task.id);
          queryClient.setQueryData<Task[]>(
            iceboxKey,
            exists ? prevIcebox.map(applyPatch) : [cooledTask, ...prevIcebox],
          );
        } else {
          queryClient.setQueryData<Task[]>(iceboxKey, prevIcebox.map(applyPatch));
        }
      }

      return { prevBoard, prevIcebox };
    },
    onError(error, _vars, ctx) {
      if (ctx?.prevBoard) queryClient.setQueryData(boardKey, ctx.prevBoard);
      if (ctx?.prevIcebox) queryClient.setQueryData(iceboxKey, ctx.prevIcebox);
      setActionError(error);
    },
    onSettled(_data, _error, { task }) {
      markBusy([task.id], false);
      void invalidateTags(queryClient, [
        tags.board(slug),
        tags.boards,
        tags.tasks,
        tags.task(task.id),
      ]);
    },
  });

  const rankMutation = useMutation<
    { tasks: Task[] },
    ApiFailure,
    RankVars,
    { prevBoard?: Task[] | undefined }
  >({
    mutationFn: fetching(({ taskIds }: RankVars) => api.rankBoardTasks(slug, taskIds, false)),
    async onMutate({ taskIds }) {
      setActionError(null);
      markBusy(taskIds, true);
      await queryClient.cancelQueries({ queryKey: boardKey });
      const prevBoard = queryClient.getQueryData<Task[]>(boardKey);
      if (prevBoard) {
        const rankById = new Map<string, number>();
        taskIds.forEach((id, index) => rankById.set(id, index + 1));
        queryClient.setQueryData<Task[]>(
          boardKey,
          prevBoard.map((item) => {
            const nextRank = rankById.get(item.id);
            return nextRank !== undefined ? { ...item, rank: nextRank } : item;
          }),
        );
      }
      return { prevBoard };
    },
    onError(error, _vars, ctx) {
      if (ctx?.prevBoard) queryClient.setQueryData(boardKey, ctx.prevBoard);
      setActionError(error);
    },
    onSettled(_data, _error, { taskIds }) {
      markBusy(taskIds, false);
      void invalidateTags(queryClient, [
        tags.board(slug),
        tags.boards,
        ...taskIds.map((id) => tags.task(id)),
      ]);
    },
  });

  const setFilter = (key: keyof BoardSearch, value: string) =>
    void navigate({
      search: { ...search, [key]: value || undefined },
      replace: true,
    });

  const setTab = (next: BoardTab) =>
    void navigate({
      search: { ...search, tab: next === "up-next" ? undefined : next },
      replace: true,
    });

  const filtered =
    filters.q || filters.priority || filters.owner || filters.readyOnly || filters.parent;

  const tiers = useMemo(
    () => (boardTasks.data ? partitionBoardTiers(boardTasks.data, filters, now) : null),
    [boardTasks.data, filters, now],
  );

  // Merge lazily fetched Icebox tasks with any cooled tasks that arrived in the main list.
  const iceboxTasks = useMemo(() => {
    const byId = new Map<string, Task>();
    for (const item of tiers?.iceboxInline ?? []) byId.set(item.id, item);
    for (const item of iceboxQuery.data ?? []) {
      if (matchesTaskFilters(item, { ...filters, readyOnly: false })) {
        byId.set(item.id, item);
      }
    }
    return [...byId.values()].sort(
      (a, b) => Date.parse(b.cooled_at ?? b.updated_at) - Date.parse(a.cooled_at ?? a.updated_at),
    );
  }, [tiers?.iceboxInline, iceboxQuery.data, filters]);

  const handlePinTop = (task: Task) => {
    const allTasks = boardTasks.data ?? [];
    const order = buildPinTopOrder(task, allTasks);
    rankMutation.mutate({ taskIds: order, priority: task.priority });
  };

  const handleReorder = (draggedId: string, targetId: string, position: "before" | "after") => {
    const allTasks = boardTasks.data ?? [];
    const visibleFrontier = tiers?.upNext ?? [];
    const order = buildReorderOrder(draggedId, targetId, position, visibleFrontier, allTasks);
    if (!order || order.length === 0) return;
    const dragged = allTasks.find((item) => item.id === draggedId);
    if (!dragged) return;
    rankMutation.mutate({ taskIds: order, priority: dragged.priority });
  };

  const baseActions: TaskRowActions = {
    busyIds,
    onPinTop: handlePinTop,
    onPriorityChange: (task, priority) => updateMutation.mutate({ task, patch: { priority } }),
    onCoolToggle: (task, cooled) => updateMutation.mutate({ task, patch: { cooled } }),
    onDrop: (task, reason) => updateMutation.mutate({ task, patch: { status: "dropped", reason } }),
  };

  const frontierActions: TaskRowActions = {
    ...baseActions,
    allowReorder: true,
    onReorder: handleReorder,
  };

  const showFilterBar =
    filtered ||
    (tasks.data === undefined && !tasks.error) ||
    (tasks.data !== undefined &&
      (showsTasks(tasks.data, now) ||
        (!filters.parent && activeTab === "icebox" && iceboxTasks.length > 0)));

  const hasAnyNonIceboxTask =
    tiers !== null &&
    (tiers.doing.length > 0 ||
      tiers.review.length > 0 ||
      tiers.blocked.length > 0 ||
      tiers.upNext.length > 0 ||
      tiers.proposals.length > 0 ||
      tiers.backlogReady.length > 0 ||
      tiers.waiting.length > 0 ||
      tiers.done.length > 0);

  return (
    <>
      <Crumbs current={name}>
        <Link to="/">Home</Link>
      </Crumbs>
      <PageHeader title={name} sub={board?.goal && <PlainText text={board.goal} />}>
        {board && board.title !== slug && (
          <span className="ident text-meta text-muted-foreground">{slug}</span>
        )}
        {board?.autopilot && (
          <Badge variant="accent" shape="tag">
            <Icon name="bolt" />
            Autopilot
          </Badge>
        )}
        {board?.kind === "inbox" && (
          <Badge variant="outline" shape="tag">
            <Icon name="inbox" />
            Inbox
          </Badge>
        )}
        {board?.curate_every_secs !== null &&
          board?.curate_every_secs !== undefined &&
          board.curate_every_secs > 0 && (
            <Badge
              variant="secondary"
              shape="tag"
              title={`Curate pass every ${formatCadenceSecs(board.curate_every_secs)}`}
            >
              curate {formatCadenceSecs(board.curate_every_secs)}
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
          Temporary: the server closes this board once it is idle (never while it is paused), and
          deletes it some time after that.
        </p>
      )}

      <BoardAsks slug={slug} />

      {actionError && (
        <div className="mb-4 flex items-start gap-2">
          <div className="min-w-0 flex-1">
            <ErrorNotice error={actionError} compact />
          </div>
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            className="mt-2"
            aria-label="Dismiss error"
            onClick={() => setActionError(null)}
          >
            <Icon name="cross" className="size-3.5" />
          </Button>
        </div>
      )}

      {filters.parent && (
        <ParentFilter
          id={filters.parent}
          slug={slug}
          search={search}
          parent={boardTasks.data?.find((task) => task.id === filters.parent)}
          subtasks={childList.data}
        />
      )}

      {!filters.parent && tiers && (
        <BoardTierTabs
          active={activeTab}
          upNextCount={tiers.upNext.length}
          proposalsCount={tiers.proposals.length}
          backlogCount={tiers.backlogReady.length + tiers.waiting.length}
          iceboxCount={
            iceboxQuery.data !== undefined || tiers.iceboxInline.length > 0
              ? iceboxTasks.length
              : null
          }
          onChange={setTab}
        />
      )}

      {/* While tasks load the bar shows disabled, so the header holds still. A board with
          nothing to filter, or whose tasks failed to load, has no bar: it would only look
          broken there. */}
      {showFilterBar && (
        <form
          className="mb-5 flex flex-wrap items-center gap-2 max-md:grid max-md:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto]"
          role="search"
          onSubmit={(event) => event.preventDefault()}
        >
          <fieldset
            className="contents"
            disabled={
              tasks.data === undefined ||
              (!filtered && !showsTasks(tasks.data, now) && activeTab !== "icebox")
            }
          >
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

      {filters.parent ? (
        <QueryFrame query={tasks} label="Loading tasks">
          {(list) => {
            const groups = groupTasks(list, filters, now).filter((group) => group.tasks.length > 0);
            if (groups.length === 0) {
              return <Empty title="No tasks match these filters" icon="search" />;
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
                    collapsed={collapsed.has(group.key)}
                    onToggle={() => toggle(group.key)}
                  >
                    <TaskList
                      tasks={group.tasks}
                      agents={agents}
                      done={done}
                      now={now}
                      actions={baseActions}
                    />
                  </TaskGroupSection>
                ))}
              </TaskBoard>
            );
          }}
        </QueryFrame>
      ) : activeTab === "icebox" ? (
        <QueryFrame query={iceboxQuery} label="Loading icebox">
          {() => {
            if (iceboxTasks.length === 0) {
              return filtered ? (
                <Empty title="No icebox tasks match these filters" icon="search" />
              ) : (
                <Empty title="Icebox is empty" icon="icebox">
                  Cooled low-priority tasks appear here after 30 days idle or when you send a task
                  to Icebox.
                </Empty>
              );
            }
            return (
              <TaskBoard>
                <TaskGroupSection
                  id="group-icebox"
                  heading={
                    <>
                      <Icon name="icebox" className="size-3.5 text-muted-foreground" />
                      Icebox
                    </>
                  }
                  count={iceboxTasks.length}
                >
                  <TaskList
                    tasks={iceboxTasks}
                    agents={agents}
                    done={done}
                    now={now}
                    actions={baseActions}
                  />
                </TaskGroupSection>
              </TaskBoard>
            );
          }}
        </QueryFrame>
      ) : (
        <QueryFrame query={boardTasks} label="Loading tasks">
          {() => {
            if (!tiers) return null;

            if (activeTab === "proposals") {
              if (tiers.proposals.length === 0) {
                return filtered ? (
                  <Empty title="No proposals match these filters" icon="search" />
                ) : (
                  <Empty title="No pending proposals" icon="inbox">
                    Pending <code>Proposal:</code> tasks and manual tasks appear here sorted by
                    priority and report count.
                  </Empty>
                );
              }
              return (
                <TaskBoard>
                  <TaskGroupSection
                    id="group-proposals"
                    heading={
                      <>
                        <Icon name="inbox" className="size-3.5 text-amber" />
                        Proposals
                      </>
                    }
                    count={tiers.proposals.length}
                  >
                    <TaskList
                      tasks={tiers.proposals}
                      agents={agents}
                      done={done}
                      now={now}
                      actions={baseActions}
                    />
                  </TaskGroupSection>
                </TaskBoard>
              );
            }

            if (activeTab === "backlog") {
              if (tiers.backlogReady.length === 0 && tiers.waiting.length === 0) {
                return filtered ? (
                  <Empty title="No backlog tasks match these filters" icon="search" />
                ) : (
                  <Empty title="Backlog is empty" icon="task">
                    Ready tasks beyond the Top-{UP_NEXT_FRONTIER_SIZE} Up Next frontier and tasks
                    waiting on blockers or a start date appear here.
                  </Empty>
                );
              }
              return (
                <TaskBoard>
                  {tiers.backlogReady.length > 0 && (
                    <TaskGroupSection
                      id="group-backlog"
                      heading={
                        <>
                          <StatusIcon status="ready" />
                          Backlog (beyond #{UP_NEXT_FRONTIER_SIZE})
                        </>
                      }
                      count={tiers.backlogReady.length}
                      collapsed={collapsed.has("backlog")}
                      onToggle={() => toggle("backlog")}
                    >
                      <TaskList
                        tasks={tiers.backlogReady}
                        agents={agents}
                        done={done}
                        now={now}
                        actions={baseActions}
                      />
                    </TaskGroupSection>
                  )}
                  {tiers.waiting.length > 0 && (
                    <TaskGroupSection
                      id="group-open"
                      heading={
                        <>
                          <StatusIcon status="open" />
                          Waiting
                        </>
                      }
                      count={tiers.waiting.length}
                      collapsed={collapsed.has("open")}
                      onToggle={() => toggle("open")}
                    >
                      <TaskList
                        tasks={tiers.waiting}
                        agents={agents}
                        done={done}
                        now={now}
                        actions={baseActions}
                      />
                    </TaskGroupSection>
                  )}
                </TaskBoard>
              );
            }

            // Default tab: Up Next (Top-20 Frontier) + active work strip + collapsed Done / Dropped.
            const activeAndUpNextEmpty =
              tiers.doing.length === 0 &&
              tiers.review.length === 0 &&
              tiers.blocked.length === 0 &&
              tiers.upNext.length === 0 &&
              tiers.done.length === 0 &&
              tiers.dropped.length === 0;

            if (activeAndUpNextEmpty) {
              if (filtered) return <Empty title="No tasks match these filters" icon="search" />;
              if (!hasAnyNonIceboxTask) {
                return (
                  <Empty title="No tasks on this board yet" icon="task">
                    Agents add tasks with <code>task_add</code>; they appear here as they do.
                  </Empty>
                );
              }
              return (
                <Empty title="Nothing ready in Up Next" icon="task">
                  {tiers.proposals.length > 0 && `${tiers.proposals.length} in Proposals`}
                  {tiers.proposals.length > 0 &&
                    tiers.backlogReady.length + tiers.waiting.length > 0 &&
                    " · "}
                  {tiers.backlogReady.length + tiers.waiting.length > 0 &&
                    `${tiers.backlogReady.length + tiers.waiting.length} in Backlog`}
                </Empty>
              );
            }

            return (
              <TaskBoard>
                {tiers.doing.length > 0 && (
                  <TaskGroupSection
                    id="group-doing"
                    heading={
                      <>
                        <StatusIcon status="doing" />
                        Doing
                      </>
                    }
                    count={tiers.doing.length}
                    collapsed={collapsed.has("doing")}
                    onToggle={() => toggle("doing")}
                  >
                    <TaskList
                      tasks={tiers.doing}
                      agents={agents}
                      done={done}
                      now={now}
                      actions={baseActions}
                    />
                  </TaskGroupSection>
                )}
                {tiers.review.length > 0 && (
                  <TaskGroupSection
                    id="group-review"
                    heading={
                      <>
                        <StatusIcon status="review" />
                        Review
                      </>
                    }
                    count={tiers.review.length}
                    collapsed={collapsed.has("review")}
                    onToggle={() => toggle("review")}
                  >
                    <TaskList
                      tasks={tiers.review}
                      agents={agents}
                      done={done}
                      now={now}
                      actions={baseActions}
                    />
                  </TaskGroupSection>
                )}
                {tiers.blocked.length > 0 && (
                  <TaskGroupSection
                    id="group-blocked"
                    heading={
                      <>
                        <StatusIcon status="blocked" />
                        Blocked
                      </>
                    }
                    count={tiers.blocked.length}
                    collapsed={collapsed.has("blocked")}
                    onToggle={() => toggle("blocked")}
                  >
                    <TaskList
                      tasks={tiers.blocked}
                      agents={agents}
                      done={done}
                      now={now}
                      actions={baseActions}
                    />
                  </TaskGroupSection>
                )}
                {tiers.upNext.length > 0 && (
                  <TaskGroupSection
                    id="group-ready"
                    heading={
                      <>
                        <StatusIcon status="ready" />
                        Up Next
                      </>
                    }
                    count={
                      tiers.backlogReady.length > 0
                        ? `${tiers.upNext.length} of ${tiers.totalReadyAuto}`
                        : tiers.upNext.length
                    }
                    collapsed={collapsed.has("ready")}
                    onToggle={() => toggle("ready")}
                    aside={
                      tiers.backlogReady.length > 0 ? (
                        <button
                          type="button"
                          className="inline-flex min-h-7 cursor-pointer items-center gap-1 text-meta font-medium text-link hover:underline max-md:min-h-11"
                          onClick={() => setTab("backlog")}
                        >
                          +{tiers.backlogReady.length} in Backlog{" "}
                          <Icon name="arrow" className="size-3.5" />
                        </button>
                      ) : undefined
                    }
                  >
                    <TaskList
                      tasks={tiers.upNext}
                      agents={agents}
                      done={done}
                      now={now}
                      actions={frontierActions}
                    />
                  </TaskGroupSection>
                )}
                {tiers.done.length > 0 && (
                  <TaskGroupSection
                    id="group-done"
                    heading={
                      <>
                        <StatusIcon status="done" />
                        Done in the last 30 days
                      </>
                    }
                    count={tiers.done.length}
                    collapsed={collapsed.has("done")}
                    onToggle={() => toggle("done")}
                  >
                    <TaskList tasks={tiers.done} agents={agents} done={done} now={now} />
                  </TaskGroupSection>
                )}
                {tiers.dropped.length > 0 && (
                  <TaskGroupSection
                    id="group-dropped"
                    heading={
                      <>
                        <StatusIcon status="dropped" />
                        Dropped
                      </>
                    }
                    count={tiers.dropped.length}
                    collapsed={collapsed.has("dropped")}
                    onToggle={() => toggle("dropped")}
                  >
                    <TaskList tasks={tiers.dropped} agents={agents} done={done} now={now} />
                  </TaskGroupSection>
                )}
              </TaskBoard>
            );
          }}
        </QueryFrame>
      )}
    </>
  );
}

const BOARD_TABS: readonly {
  id: BoardTab;
  label: string;
  description: string;
}[] = [
  { id: "up-next", label: "Up Next", description: "Active work and Top-20 ready frontier" },
  { id: "proposals", label: "Proposals", description: "Pending proposals and manual tasks" },
  { id: "backlog", label: "Backlog", description: "Ready tasks beyond #20 and waiting tasks" },
  { id: "icebox", label: "Icebox", description: "Cooled tasks ready to revive on demand" },
];

function BoardTierTabs({
  active,
  upNextCount,
  proposalsCount,
  backlogCount,
  iceboxCount,
  onChange,
}: {
  active: BoardTab;
  upNextCount: number;
  proposalsCount: number;
  backlogCount: number;
  iceboxCount: number | null;
  onChange: (tab: BoardTab) => void;
}) {
  const counts: Record<BoardTab, number | null> = {
    "up-next": upNextCount,
    proposals: proposalsCount,
    backlog: backlogCount,
    icebox: iceboxCount,
  };

  return (
    <div
      className="mb-4 flex items-center gap-1 overflow-x-auto border-b"
      role="tablist"
      aria-label="Board tiers"
      onKeyDown={(event) => {
        if (event.key !== "ArrowRight" && event.key !== "ArrowLeft") return;
        const currentIndex = BOARD_TABS.findIndex((tab) => tab.id === active);
        if (currentIndex === -1) return;
        event.preventDefault();
        const delta = event.key === "ArrowRight" ? 1 : -1;
        const nextTab = BOARD_TABS[(currentIndex + delta + BOARD_TABS.length) % BOARD_TABS.length];
        if (nextTab) onChange(nextTab.id);
      }}
    >
      {BOARD_TABS.map((tab) => {
        const selected = active === tab.id;
        const count = counts[tab.id];
        return (
          <button
            key={tab.id}
            type="button"
            role="tab"
            id={`tab-${tab.id}`}
            aria-selected={selected}
            title={tab.description}
            className={cn(
              "-mb-px inline-flex min-h-9 cursor-pointer items-center gap-1.5 border-b-2 px-3 py-1.5 text-sm whitespace-nowrap transition-colors focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-ring max-md:gap-1 max-md:px-2 max-md:text-xs",
              selected
                ? "border-signal font-semibold text-foreground"
                : "border-transparent font-medium text-muted-foreground hover:text-foreground",
            )}
            onClick={() => onChange(tab.id)}
          >
            {tab.id === "icebox" && <Icon name="icebox" className="size-3.5" />}
            <span>{tab.label}</span>
            {count !== null && (
              <span
                className={cn(
                  "inline-grid h-4.5 min-w-5 place-items-center rounded-full px-1.5 text-2xs leading-none font-semibold tabular-nums",
                  tab.id === "proposals" && count > 0
                    ? "bg-amber-soft text-amber ring-1 ring-amber/30 ring-inset"
                    : selected
                      ? "bg-primary-soft text-link ring-1 ring-signal/30 ring-inset"
                      : "bg-secondary text-subtle-foreground",
                )}
              >
                {count}
              </span>
            )}
          </button>
        );
      })}
    </div>
  );
}

/** How many of a board's open asks its page lists; Needs you, filtered to the board, has all. */
export const BOARD_ASK_ROWS = 5;

/**
 * The board's open asks above its tasks: what the board waits on the user for comes first.
 * Nothing shows while the asks load or when none is open.
 */
function BoardAsks({ slug }: { slug: string }) {
  const open = useOpenAsks().data;
  const asks = useMemo(() => (open ?? []).filter((ask) => ask.board === slug), [open, slug]);
  if (asks.length === 0) return null;
  return (
    <div className="mb-5">
      <TaskBoard>
        <TaskGroupSection
          id="board-asks"
          heading={
            <>
              <Icon name="inbox" className="size-3.5 text-amber" />
              Waiting on you
            </>
          }
          count={asks.length}
          aside={
            asks.length > BOARD_ASK_ROWS && (
              <Link
                className="inline-flex min-h-7 items-center gap-1 text-meta font-medium max-md:min-h-11"
                to="/asks"
                search={{ board: slug }}
              >
                All {asks.length} asks <Icon name="arrow" className="size-3.5" />
              </Link>
            )
          }
        >
          <AskRows asks={asks} limit={BOARD_ASK_ROWS} empty="" inFrame />
        </TaskGroupSection>
      </TaskBoard>
    </div>
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
  return <div className="edge divide-y overflow-hidden border-y bg-card">{children}</div>;
}

/**
 * A group of tasks under a shaded heading with its count. With `onToggle`, the heading is a
 * button that folds the group away.
 */
export function TaskGroupSection({
  id,
  heading,
  count,
  collapsed = false,
  onToggle,
  level = 2,
  aside,
  children,
}: {
  id: string;
  heading: ReactNode;
  /** The number of tasks, or "1 of 2" where only some are listed. */
  count: number | string;
  collapsed?: boolean;
  onToggle?: () => void;
  /** 3 where the groups sit inside a titled section, as on the home page. */
  level?: 2 | 3;
  /** A link at the heading's right end, outside the heading itself. */
  aside?: ReactNode;
  children: ReactNode;
}) {
  const bodyId = useId();
  const Heading = level === 2 ? "h2" : "h3";
  const row = "flex h-9.5 w-full items-center gap-2 px-(--gutter)";
  const label = (
    <>
      {heading}
      <span className="text-meta font-medium text-subtle-foreground tabular-nums">{count}</span>
    </>
  );
  const headingEl = (
    <Heading
      id={id}
      className={cn(
        "text-sm font-semibold",
        !aside && "bg-muted/45",
        !aside && !collapsed && "border-b",
        aside && "min-w-0 flex-1",
        !onToggle && row,
      )}
    >
      {onToggle ? (
        <button
          type="button"
          className={cn(
            row,
            "group/toggle cursor-pointer text-left hover:bg-sidebar-accent focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-ring",
          )}
          aria-expanded={!collapsed}
          aria-controls={bodyId}
          onClick={onToggle}
        >
          {/* Left of the label, as on the home page's sections. */}
          <Icon
            name="chevronDown"
            className={cn(
              "size-3.5 flex-none text-muted-foreground transition-transform group-hover/toggle:text-foreground motion-reduce:transition-none",
              collapsed && "-rotate-90",
            )}
          />
          {label}
        </button>
      ) : (
        label
      )}
    </Heading>
  );
  return (
    <section aria-labelledby={id}>
      {aside ? (
        <div
          className={cn(
            "flex items-center gap-3 bg-muted/45 pr-(--gutter)",
            !collapsed && "border-b",
          )}
        >
          {headingEl}
          {aside}
        </div>
      ) : (
        headingEl
      )}
      <div id={bodyId} hidden={collapsed}>
        {children}
      </div>
    </section>
  );
}

interface TaskRowData {
  task: Task;
  agents: Map<string, Agent>;
  /** Ids of done tasks, so finished blockers drop out; unset where done tasks are not loaded. */
  done: ReadonlySet<string> | undefined;
  now: number;
  actions?: TaskRowActions | undefined;
  prevSamePriorityId?: string | undefined;
  nextSamePriorityId?: string | undefined;
  dropping: boolean;
  onStartDrop: () => void;
}

const features = tableFeatures({});
const column = createColumnHelper<typeof features, TaskRowData>();

/** One task per row: priority, status, id, title with its tags, owner, optional actions, and last update. */
const baseColumns = column.columns([
  column.display({
    id: "priority",
    cell: ({ row }) => <PriorityBadge priority={row.original.task.priority} />,
  }),
  column.display({
    id: "status",
    cell: ({ row }) => {
      const { task } = row.original;
      return <StatusIcon status={statusKey(task)} label />;
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

const actionColumns = column.columns([
  ...baseColumns.slice(0, 5),
  column.display({
    id: "actions",
    cell: ({ row }) => <RowOperatorControls {...row.original} />,
  }),
  baseColumns[5]!,
]);

// Desktop rows share the list's columns (subgrid), so the id column fits the longest id.
// On a phone a row stacks: status and title, then id, priority, owner, and operator actions.
const CELL_CLASS: Record<string, string> = {
  priority: "max-md:[grid-area:prio] max-md:self-center",
  status: "max-md:[grid-area:status] max-md:mt-0.75",
  id: "ident text-subtle-foreground text-xs max-md:[grid-area:id] max-md:self-center",
  main: "flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 max-md:[grid-area:main]",
  owner:
    "flex min-w-0 items-center overflow-hidden max-md:[grid-area:owner] max-md:text-meta max-md:flex max-md:flex-wrap max-md:items-center max-md:[&_[data-slot=avatar]]:hidden",
  actions:
    "relative z-1 flex items-center justify-end gap-1 max-md:[grid-area:actions] max-md:justify-start max-md:flex-wrap max-md:pt-1",
  updated:
    "text-meta text-muted-foreground text-right whitespace-nowrap tabular-nums max-md:[grid-area:updated] max-md:pt-0.75",
};

function RowOperatorControls({
  task,
  actions,
  prevSamePriorityId,
  nextSamePriorityId,
  dropping,
  onStartDrop,
}: TaskRowData) {
  if (!actions || task.status === "done" || task.status === "dropped") return null;
  const busy = actions.busyIds.has(task.id);
  const isOpen = task.status === "open";
  const isCooled = task.cooled_at !== null;
  const canReorder = Boolean(actions.allowReorder && actions.onReorder && isOpen && !isCooled);

  return (
    <div
      className="flex flex-wrap items-center gap-1 opacity-80 transition-opacity group-focus-within/row:opacity-100 group-hover/row:opacity-100 max-md:opacity-100"
      data-slot="row-controls"
    >
      {canReorder && (
        <div className="inline-flex items-center gap-0.5">
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            disabled={busy || !prevSamePriorityId}
            aria-label={`Move ${task.id} up`}
            title="Move up in priority level"
            onClick={() => {
              if (prevSamePriorityId) actions.onReorder?.(task.id, prevSamePriorityId, "before");
            }}
          >
            <Icon name="arrowUp" className="size-3" />
          </Button>
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            disabled={busy || !nextSamePriorityId}
            aria-label={`Move ${task.id} down`}
            title="Move down in priority level"
            onClick={() => {
              if (nextSamePriorityId) actions.onReorder?.(task.id, nextSamePriorityId, "after");
            }}
          >
            <Icon name="arrowDown" className="size-3" />
          </Button>
        </div>
      )}

      {isOpen && !isCooled && (
        <Button
          type="button"
          variant="outline"
          size="xs"
          disabled={busy || task.rank === 1}
          aria-label={`Pin ${task.id} to #1`}
          title={task.rank === 1 ? "Already pinned at #1" : "Pin to #1 of priority level"}
          onClick={() => actions.onPinTop(task)}
        >
          <Icon name="pinTop" className="size-3" />
          <span>Top</span>
        </Button>
      )}

      {!isCooled && (
        <PriorityQuickPicker
          taskId={task.id}
          value={task.priority}
          disabled={busy}
          compact
          onSelect={(priority) => actions.onPriorityChange(task, priority)}
        />
      )}

      {isOpen && (
        <Button
          type="button"
          variant={isCooled ? "secondary" : "ghost"}
          size="xs"
          disabled={busy}
          aria-label={isCooled ? `Revive ${task.id} from Icebox` : `Move ${task.id} to Icebox`}
          title={isCooled ? "Revive back to active backlog" : "Cool to Icebox"}
          onClick={() => actions.onCoolToggle(task, !isCooled)}
        >
          <Icon name={isCooled ? "revive" : "icebox"} className="size-3" />
          <span>{isCooled ? "Revive" : "Icebox"}</span>
        </Button>
      )}

      <Button
        type="button"
        variant="ghost"
        size="xs"
        disabled={busy || dropping}
        className="text-muted-foreground hover:text-destructive"
        aria-label={`Drop ${task.id}`}
        title="Drop task with reason"
        onClick={onStartDrop}
      >
        Drop
      </Button>
    </div>
  );
}

export function TaskList({
  tasks,
  agents,
  done,
  now,
  actions,
}: {
  tasks: Task[];
  agents: Map<string, Agent>;
  done?: ReadonlySet<string>;
  now: number;
  actions?: TaskRowActions | undefined;
}) {
  const [droppingId, setDroppingId] = useState<string | null>(null);
  const [draggedId, setDraggedId] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<{
    id: string;
    position: "before" | "after";
  } | null>(null);

  const hasActions = Boolean(actions);
  const allowReorder = Boolean(actions?.allowReorder && actions?.onReorder);

  // Precompute adjacent same-priority neighbor IDs for keyboard Up/Down controls.
  const neighbors = useMemo(() => {
    const map = new Map<string, { prev?: string; next?: string }>();
    if (!allowReorder) return map;
    const byPriority = new Map<Priority, Task[]>();
    for (const task of tasks) {
      if (task.status !== "open" || task.cooled_at !== null) continue;
      const list = byPriority.get(task.priority) ?? [];
      list.push(task);
      byPriority.set(task.priority, list);
    }
    for (const list of byPriority.values()) {
      list.forEach((task, index) => {
        const entry: { prev?: string; next?: string } = {};
        if (index > 0) entry.prev = list[index - 1]!.id;
        if (index + 1 < list.length) entry.next = list[index + 1]!.id;
        map.set(task.id, entry);
      });
    }
    return map;
  }, [tasks, allowReorder]);

  const data = useMemo(
    () =>
      tasks.map((task) => ({
        task,
        agents,
        done,
        now,
        actions,
        prevSamePriorityId: neighbors.get(task.id)?.prev,
        nextSamePriorityId: neighbors.get(task.id)?.next,
        dropping: droppingId === task.id,
        onStartDrop: () => setDroppingId(task.id),
      })),
    [tasks, agents, done, now, actions, neighbors, droppingId],
  );
  const columns = hasActions ? actionColumns : baseColumns;
  const table = useTable({ features, columns, data, getRowId: (row) => row.task.id });

  const draggedTask = draggedId ? tasks.find((item) => item.id === draggedId) : undefined;

  return (
    // A row's gutter padding sits inside the subgrid's first and last tracks, so those tracks
    // add the gutter to their own width; otherwise priority would run into the status mark.
    <ul
      className={cn(
        "grid gap-x-3.5 divide-y max-md:block",
        hasActions
          ? "grid-cols-[calc(var(--gutter)+40px)_14px_minmax(84px,max-content)_minmax(0,1fr)_132px_max-content_calc(var(--gutter)+68px)]"
          : "grid-cols-[calc(var(--gutter)+40px)_14px_minmax(84px,max-content)_minmax(0,1fr)_132px_calc(var(--gutter)+80px)]",
      )}
    >
      {table.getRowModel().rows.map((row) => {
        const { task, dropping } = row.original;
        const busy = actions?.busyIds.has(task.id) ?? false;
        const canDrag = allowReorder && task.status === "open" && task.cooled_at === null && !busy;
        const isValidTarget =
          canDrag &&
          draggedTask !== undefined &&
          draggedTask.id !== task.id &&
          draggedTask.priority === task.priority;
        const indicator = dropTarget?.id === task.id ? dropTarget.position : null;

        return (
          <li
            key={row.id}
            draggable={canDrag}
            onDragStart={(event) => {
              if (!canDrag) return;
              setDraggedId(task.id);
              event.dataTransfer.setData("text/plain", task.id);
              event.dataTransfer.effectAllowed = "move";
            }}
            onDragOver={(event) => {
              if (!isValidTarget) return;
              event.preventDefault();
              event.dataTransfer.dropEffect = "move";
              const rect = event.currentTarget.getBoundingClientRect();
              const position = event.clientY < rect.top + rect.height / 2 ? "before" : "after";
              setDropTarget((prev) =>
                prev?.id === task.id && prev.position === position
                  ? prev
                  : { id: task.id, position },
              );
            }}
            onDragLeave={(event) => {
              if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
                setDropTarget((prev) => (prev?.id === task.id ? null : prev));
              }
            }}
            onDrop={(event) => {
              if (!isValidTarget || !draggedId) return;
              event.preventDefault();
              const position = dropTarget?.id === task.id ? dropTarget.position : "before";
              actions?.onReorder?.(draggedId, task.id, position);
              setDraggedId(null);
              setDropTarget(null);
            }}
            onDragEnd={() => {
              setDraggedId(null);
              setDropTarget(null);
            }}
            className={cn(
              "group/row relative col-span-full grid min-h-9 grid-cols-subgrid items-center gap-x-3 px-(--gutter) py-1 hover:bg-muted max-md:grid-cols-[14px_auto_auto_minmax(0,1fr)_auto] max-md:items-start max-md:gap-x-2.5 max-md:gap-y-1.5 max-md:py-2.5",
              hasActions
                ? "max-md:[grid-template-areas:'status_main_main_main_updated'_'._id_prio_owner_owner'_'._actions_actions_actions_actions']"
                : "max-md:[grid-template-areas:'status_main_main_main_updated'_'._id_prio_owner_owner']",
              draggedId === task.id && "opacity-50",
              indicator === "before" && "shadow-[inset_0_2px_0_0_var(--color-signal)]",
              indicator === "after" && "shadow-[inset_0_-2px_0_0_var(--color-signal)]",
            )}
          >
            {canDrag && (
              <span
                className="pointer-events-none absolute top-1/2 left-2 z-1 -translate-y-1/2 text-muted-foreground/60 group-hover/row:text-foreground max-md:hidden"
                aria-hidden="true"
              >
                <Icon name="grip" className="size-3.5" />
              </span>
            )}
            {row.getAllCells().map((cell) => (
              <div key={cell.id} className={CELL_CLASS[cell.column.id]}>
                <table.FlexRender cell={cell} />
              </div>
            ))}
            {dropping && actions && (
              <div className="col-span-full pt-1.5 pb-1">
                <InlineDropForm
                  taskId={task.id}
                  pending={busy}
                  onConfirm={(reason) => {
                    actions.onDrop(task, reason);
                    setDroppingId(null);
                  }}
                  onCancel={() => setDroppingId(null)}
                />
              </div>
            )}
          </li>
        );
      })}
    </ul>
  );
}

/** Above the row link's cover, so its links stay clickable. */
const DEPENDENCY =
  "relative z-1 inline-flex min-h-6 items-center gap-1 text-meta text-muted-foreground";

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
        className="min-w-0 py-0.5 font-medium text-foreground after:absolute after:inset-0"
        to="/tasks/$id"
        params={{ id: task.id }}
      >
        <PlainText className="whitespace-normal" text={task.title} />
      </Link>
      <TaskSignalBadges task={task} />
      {/* What the task waits for is muted text with an icon; outlined chips are tags. */}
      {blockers.length > 0 && (
        <span className={DEPENDENCY} data-dependency="blockers">
          <Icon name="link" className="size-3.5 flex-none" />
          <span>
            blocked by{" "}
            {blockers.map((id, index) => (
              <Fragment key={id}>
                {index > 0 && ", "}
                <Link
                  className="ident inline-flex min-h-6 items-center text-foreground"
                  to="/tasks/$id"
                  params={{ id }}
                >
                  {id}
                </Link>
              </Fragment>
            ))}
          </span>
        </span>
      )}
      {/* Nothing it waits on is still in progress, and the board moves nothing itself. */}
      {task.blockers_closed && (
        <span className={cn(DEPENDENCY, "text-amber")} data-dependency="closed">
          <Icon name="link" className="size-3.5 flex-none" />
          {BLOCKERS_CLOSED}
        </span>
      )}
      {/* An umbrella task: its children, narrowed to on this list. */}
      {task.child_count > 0 && (
        <Link
          className={cn(DEPENDENCY, "hover:text-foreground")}
          to="/boards/$slug"
          params={{ slug: task.board }}
          search={{ parent: task.id }}
          data-dependency="children"
        >
          <Icon name="subtasks" className="size-3.5 flex-none" />
          {task.child_count} {task.child_count === 1 ? "subtask" : "subtasks"}
        </Link>
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
        <span className={DEPENDENCY} data-dependency="date">
          <Icon name="clock" className="size-3.5 flex-none" />
          <span>
            waits until <DayTime at={waitsUntil} now={now} />
          </span>
        </span>
      )}
      {task.cooled_at && (
        <span className={DEPENDENCY} data-dependency="cooled">
          <span>
            cooled <RelTime at={task.cooled_at} />
          </span>
        </span>
      )}
      {task.blocked_reason && (
        <p className="basis-full text-meta text-status-blocked">
          <span className="font-semibold">Blocked:</span> <PlainText text={task.blocked_reason} />
        </p>
      )}
    </>
  );
}

/**
 * The parent the list is narrowed to: its id and title, how its children stand, and a link
 * back to every task.
 */
function ParentFilter({
  id,
  slug,
  search,
  parent,
  subtasks,
}: {
  id: string;
  slug: string;
  search: BoardSearch;
  /** The parent as the board list has it, for its title; absent until that list loads. */
  parent: Task | undefined;
  /** Its children; absent while they load. */
  subtasks: Task[] | undefined;
}) {
  return (
    <section
      className="mb-5 grid gap-2.5 rounded-lg border bg-card px-4 py-3 shadow-raise"
      aria-labelledby="parent-filter"
    >
      <div className="flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1">
        <h2 id="parent-filter" className="min-w-0 text-sm font-semibold">
          <span className="font-normal text-muted-foreground">Subtasks of </span>
          <Link to="/tasks/$id" params={{ id }}>
            <span className="ident">{id}</span>
            {parent && (
              <>
                {" "}
                <PlainText text={parent.title} className="whitespace-normal" />
              </>
            )}
          </Link>
        </h2>
        <Link
          className="inline-flex min-h-7 items-center gap-1 text-meta font-medium max-md:min-h-11"
          to="/boards/$slug"
          params={{ slug }}
          search={{ ...search, parent: undefined }}
          replace
        >
          <Icon name="cross" className="size-3.5" />
          Show all tasks
        </Link>
      </div>
      {subtasks !== undefined && (
        <StatusTally counts={taskTally(subtasks)} label="Subtasks by status" empty="No subtasks" />
      )}
    </section>
  );
}
