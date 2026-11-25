import { useEffect, useRef, useState } from "react";
import { PRIORITIES, type Priority, type Task } from "@/api/schema.ts";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import { Input } from "@/components/ui/input.tsx";
import { cn } from "@/lib/utils.ts";
import { Icon } from "@/ui/icons.tsx";

/** Maximum number of ready tasks in a board's Up Next Frontier. */
export const UP_NEXT_FRONTIER_SIZE = 20;

const DONE_WINDOW_MS = 30 * 86_400_000;

export interface TierFilters {
  q: string;
  priority: string;
  owner: string;
  readyOnly: boolean;
  parent: string;
}

export interface BoardTiers {
  doing: Task[];
  review: Task[];
  blocked: Task[];
  /** Top-20 ready auto open tasks in exact v2 dispatch order. */
  upNext: Task[];
  /** Total matching ready auto open tasks before the Top-20 frontier slice. */
  totalReadyAuto: number;
  /** Pending `Proposal:` or `auto === false` open tasks. */
  proposals: Task[];
  /** Ready auto open tasks beyond the Top-20 Up Next Frontier. */
  backlogReady: Task[];
  /** Non-ready auto open tasks (waiting on blockers or future `not_before`). */
  waiting: Task[];
  done: Task[];
  dropped: Task[];
  /** Any cooled tasks that happened to arrive in the main task list (older/mixed servers). */
  iceboxInline: Task[];
}

export function taskNumber(task: Pick<Task, "id">): number {
  return Number(task.id.slice(task.id.lastIndexOf("-") + 1));
}

/** Effective priority via blocker Priority Inheritance, falling back to stored priority. */
export function effectivePriorityOf(task: Pick<Task, "priority" | "effective_priority">): Priority {
  return task.effective_priority ?? task.priority;
}

/** Deduplicated report count, falling back to legacy `feedback.reports` when absent. */
export function reportCountOf(task: Pick<Task, "report_count" | "feedback">): number {
  return Math.max(task.report_count, task.feedback?.reports ?? 0);
}

export function sizeWeight(size: Task["size"]): number {
  switch (size) {
    case "xs":
      return 1.0;
    case "s":
      return 1.5;
    case "m":
      return 3.0;
    case "l":
      return 6.0;
    default:
      return 2.0;
  }
}

/**
 * Deterministic unranked WSJF tie-break score matching `docs/SCHEDULER_DESIGN.md` Section 8.2:
 * `(1.0 + 0.5 * min(report_count, 10) + 0.5 * min(unblocks_count, 6)) / size_weight`.
 */
export function wsjfScore(task: Task, unblocksCount = 0): number {
  const reports = Math.min(reportCountOf(task), 10);
  const unblocks = Math.min(unblocksCount, 6);
  return (1.0 + 0.5 * reports + 0.5 * unblocks) / sizeWeight(task.size);
}

/** Count how many non-closed tasks list each task id as a blocker. */
export function buildUnblocksMap(tasks: readonly Task[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const task of tasks) {
    if (task.status === "done" || task.status === "dropped") continue;
    for (const blocker of task.blockers) {
      counts.set(blocker, (counts.get(blocker) ?? 0) + 1);
    }
  }
  return counts;
}

/**
 * Strict v2 dispatch order:
 * 1. `effective_priority ?? priority` (`p1` < `p2` < `p3`)
 * 2. Ranked before unranked (`rank != null`)
 * 3. Lower explicit `rank` first (when both ranked)
 * 4. Higher WSJF score first (when both unranked)
 * 5. Lower task id number first
 */
export function inDispatchOrder(a: Task, b: Task, unblocks?: ReadonlyMap<string, number>): number {
  const prio = effectivePriorityOf(a).localeCompare(effectivePriorityOf(b));
  if (prio !== 0) return prio;
  const rankedDiff = Number(a.rank == null) - Number(b.rank == null);
  if (rankedDiff !== 0) return rankedDiff;
  if (a.rank !== null && b.rank !== null) {
    const rankDiff = a.rank - b.rank;
    if (rankDiff !== 0) return rankDiff;
  } else {
    const wsjfDiff =
      wsjfScore(b, unblocks?.get(b.id) ?? 0) - wsjfScore(a, unblocks?.get(a.id) ?? 0);
    if (Math.abs(wsjfDiff) > 1e-9) return wsjfDiff;
  }
  return taskNumber(a) - taskNumber(b);
}

/**
 * v2 Proposals order:
 * `(effective_priority ?? priority, report_count DESC, rank IS NULL, rank, created_at ASC, number)`.
 */
export function inProposalOrder(a: Task, b: Task): number {
  return (
    effectivePriorityOf(a).localeCompare(effectivePriorityOf(b)) ||
    reportCountOf(b) - reportCountOf(a) ||
    Number(a.rank == null) - Number(b.rank == null) ||
    (a.rank ?? 0) - (b.rank ?? 0) ||
    Date.parse(a.created_at) - Date.parse(b.created_at) ||
    taskNumber(a) - taskNumber(b)
  );
}

/** Whether an open non-cooled task belongs in the Proposals tier. */
export function isProposalTask(
  task: Pick<Task, "status" | "auto" | "proposal" | "cooled_at">,
): boolean {
  return task.status === "open" && task.cooled_at === null && (task.proposal || !task.auto);
}

export function matchesTaskFilters(task: Task, filters: TierFilters): boolean {
  const q = filters.q.trim().toLowerCase();
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
}

/**
 * Partition a board's tasks into the 4 operator tiers (Up Next Top-20 Frontier, Proposals,
 * Backlog, Icebox) plus active (`doing`/`review`/`blocked`) and closed (`done`/`dropped`).
 * Top-20 frontier membership is computed globally across the board before applying search/facet
 * filters so filtering never promotes deep backlog tasks into the frontier.
 */
export function partitionBoardTiers(
  tasks: readonly Task[],
  filters: TierFilters,
  now: number,
): BoardTiers {
  const unblocks = buildUnblocksMap(tasks);

  // Determine the authoritative board-wide Top-20 ready auto frontier before filtering.
  const allReadyAuto = tasks
    .filter(
      (task) =>
        task.status === "open" && task.cooled_at === null && task.ready && !isProposalTask(task),
    )
    .sort((a, b) => inDispatchOrder(a, b, unblocks));
  const frontierIds = new Set(allReadyAuto.slice(0, UP_NEXT_FRONTIER_SIZE).map((task) => task.id));

  const doing: Task[] = [];
  const review: Task[] = [];
  const blocked: Task[] = [];
  const upNext: Task[] = [];
  const proposals: Task[] = [];
  const backlogReady: Task[] = [];
  const waiting: Task[] = [];
  const done: Task[] = [];
  const dropped: Task[] = [];
  const iceboxInline: Task[] = [];

  for (const task of tasks) {
    if (!matchesTaskFilters(task, filters)) continue;
    if (task.status === "open" && task.cooled_at !== null) {
      iceboxInline.push(task);
      continue;
    }
    switch (task.status) {
      case "doing":
        doing.push(task);
        break;
      case "review":
        review.push(task);
        break;
      case "blocked":
        blocked.push(task);
        break;
      case "done":
        if (now - Date.parse(task.updated_at) <= DONE_WINDOW_MS) done.push(task);
        break;
      case "dropped":
        if (now - Date.parse(task.updated_at) <= DONE_WINDOW_MS) dropped.push(task);
        break;
      case "open":
        if (isProposalTask(task)) {
          proposals.push(task);
        } else if (task.ready) {
          if (frontierIds.has(task.id)) upNext.push(task);
          else backlogReady.push(task);
        } else {
          waiting.push(task);
        }
        break;
    }
  }

  const byDispatch = (a: Task, b: Task) => inDispatchOrder(a, b, unblocks);
  doing.sort(byDispatch);
  review.sort(byDispatch);
  blocked.sort(byDispatch);
  upNext.sort(byDispatch);
  proposals.sort(inProposalOrder);
  backlogReady.sort(byDispatch);
  waiting.sort(byDispatch);
  done.sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
  dropped.sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
  iceboxInline.sort(
    (a, b) => Date.parse(b.cooled_at ?? b.updated_at) - Date.parse(a.cooled_at ?? a.updated_at),
  );

  return {
    doing,
    review,
    blocked,
    upNext,
    totalReadyAuto: upNext.length + backlogReady.length,
    proposals,
    backlogReady,
    waiting,
    done,
    dropped,
    iceboxInline,
  };
}

/**
 * Build the `POST /v1/boards/{slug}/rank` task list that pins `task` to `#1` inside its stored
 * priority level while shifting all other already-ranked open tasks of that level cleanly down.
 */
export function buildPinTopOrder(task: Task, allTasks: readonly Task[]): string[] {
  const others = allTasks
    .filter(
      (item) =>
        item.id !== task.id &&
        item.status === "open" &&
        item.cooled_at === null &&
        item.priority === task.priority &&
        item.rank !== null,
    )
    .sort((a, b) => (a.rank ?? 0) - (b.rank ?? 0) || taskNumber(a) - taskNumber(b));
  return [task.id, ...others.map((item) => item.id)];
}

/**
 * Build the `POST /v1/boards/{slug}/rank` task list for moving `draggedId` before or after
 * `targetId` within their shared stored priority level and visual effective-priority band.
 * Preserves all already-ranked tasks of that level (including ones hidden by filters or in
 * another tier) so higher-ranked tasks are never demoted. Returns `null` when the move is
 * invalid or a no-op.
 */
export function buildReorderOrder(
  draggedId: string,
  targetId: string,
  position: "before" | "after",
  orderedVisibleTasks: readonly Task[],
  allTasks: readonly Task[],
): string[] | null {
  if (draggedId === targetId) return null;
  const dragged = orderedVisibleTasks.find((task) => task.id === draggedId);
  const target = orderedVisibleTasks.find((task) => task.id === targetId);
  if (!dragged || !target) return null;
  if (dragged.status !== "open" || target.status !== "open") return null;
  if (dragged.priority !== target.priority) return null;
  if (effectivePriorityOf(dragged) !== effectivePriorityOf(target)) return null;

  // Build the authoritative sequence for this stored priority level: every already-ranked open
  // non-cooled task on the board first (in rank order), followed by unranked visible tasks in
  // their current dispatch order.
  const rankedInAll = allTasks
    .filter(
      (task) =>
        task.status === "open" &&
        task.cooled_at === null &&
        task.priority === dragged.priority &&
        task.rank !== null,
    )
    .sort((a, b) => (a.rank ?? 0) - (b.rank ?? 0) || taskNumber(a) - taskNumber(b));
  const rankedIds = new Set(rankedInAll.map((task) => task.id));
  const unrankedVisible = orderedVisibleTasks.filter(
    (task) =>
      task.status === "open" &&
      task.cooled_at === null &&
      task.priority === dragged.priority &&
      !rankedIds.has(task.id),
  );
  const fullLevel = [...rankedInAll, ...unrankedVisible];

  const withoutDragged = fullLevel.filter((task) => task.id !== draggedId);
  const targetIdx = withoutDragged.findIndex((task) => task.id === targetId);
  if (targetIdx === -1) return null;
  const insertAt = position === "before" ? targetIdx : targetIdx + 1;
  const reordered = [
    ...withoutDragged.slice(0, insertAt),
    dragged,
    ...withoutDragged.slice(insertAt),
  ];

  // Check for a true no-op (order unchanged and dragged item is already ranked).
  const unchanged =
    reordered.length === fullLevel.length &&
    reordered.every((task, index) => task.id === fullLevel[index]?.id);
  if (unchanged && dragged.rank !== null) return null;

  // Include every item up through the moved item and the last already-ranked item so we never
  // create duplicate ranks or needlessly rank unranked tail items.
  const movedIndex = reordered.findIndex((task) => task.id === draggedId);
  let lastNeeded = movedIndex;
  for (let i = 0; i < reordered.length; i++) {
    if (reordered[i]?.rank !== null) lastNeeded = Math.max(lastNeeded, i);
  }
  return reordered.slice(0, lastNeeded + 1).map((task) => task.id);
}

/** Compact `#1` / `#2` rank pill for ranked tasks. */
export function RankPill({ rank, priority }: { rank: number; priority: Priority }) {
  return (
    <Badge
      variant={rank === 1 ? "accent" : "secondary"}
      shape="tag"
      className="relative z-1 font-mono text-2xs tabular-nums"
      data-badge="rank"
      title={`Rank #${rank} in ${priority.toUpperCase()}`}
    >
      #{rank}
    </Badge>
  );
}

/** Inherited priority badge shown when `effective_priority` is higher than stored `priority`. */
export function EffectivePriorityBadge({ effective, own }: { effective: Priority; own: Priority }) {
  return (
    <Badge
      variant="warn"
      shape="tag"
      className="relative z-1 font-mono text-2xs uppercase tabular-nums"
      data-badge="effective-priority"
      title={`Effective priority ${effective.toUpperCase()} (inherited from higher-priority work this task blocks; own priority ${own.toUpperCase()})`}
    >
      ↑{effective.toUpperCase()}
    </Badge>
  );
}

/** Compact inline signal badges for v2 metadata (rank, inherited priority, user origin, size, reports, icebox). */
export function TaskSignalBadges({ task }: { task: Task }) {
  const reports = reportCountOf(task);
  const inherited = task.effective_priority !== null && task.effective_priority !== task.priority;
  const knownSize = task.size !== null && task.size !== "unknown" ? task.size : null;

  return (
    <>
      {task.rank !== null && <RankPill rank={task.rank} priority={task.priority} />}
      {inherited && task.effective_priority && (
        <EffectivePriorityBadge effective={task.effective_priority} own={task.priority} />
      )}
      {task.origin === "user" && (
        <Badge
          variant="accent"
          shape="tag"
          className="relative z-1 text-2xs"
          data-badge="origin"
          title="Filed directly by the user"
        >
          User
        </Badge>
      )}
      {knownSize && (
        <Badge
          variant="outline"
          shape="tag"
          className="relative z-1 font-mono text-2xs uppercase"
          data-badge="size"
          title={`Size ${knownSize.toUpperCase()}`}
        >
          {knownSize.toUpperCase()}
        </Badge>
      )}
      {reports > 0 && (
        <Badge
          variant={reports >= 3 ? "warn" : "outline"}
          shape="tag"
          className="relative z-1 text-2xs tabular-nums"
          data-badge="reports"
          title={`${reports} ${reports === 1 ? "report" : "reports"}`}
        >
          {reports}×<span className="sr-only"> {reports === 1 ? "report" : "reports"}</span>
        </Badge>
      )}
      {task.cooled_at !== null && (
        <Badge
          variant="outline"
          shape="tag"
          className="relative z-1 text-2xs text-muted-foreground"
          data-badge="icebox"
          title={`Cooled into Icebox at ${task.cooled_at}`}
        >
          <Icon name="icebox" className="size-3" />
          Icebox
        </Badge>
      )}
    </>
  );
}

/** 1-click segmented P1 / P2 / P3 quick-picker. */
export function PriorityQuickPicker({
  taskId,
  value,
  disabled,
  compact = false,
  onSelect,
}: {
  taskId: string;
  value: Priority;
  disabled?: boolean | undefined;
  compact?: boolean | undefined;
  onSelect: (priority: Priority) => void;
}) {
  return (
    <div
      className={cn(
        "relative z-1 inline-flex items-center overflow-hidden rounded-sm border border-input bg-card",
        disabled && "opacity-60",
      )}
      role="group"
      aria-label={`Priority for ${taskId}`}
    >
      {PRIORITIES.map((level) => {
        const active = value === level;
        return (
          <button
            key={level}
            type="button"
            disabled={disabled}
            aria-pressed={active}
            aria-label={`Set ${taskId} priority to ${level.toUpperCase()}`}
            className={cn(
              "cursor-pointer font-mono font-medium uppercase transition-colors focus-visible:z-1 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-ring disabled:cursor-not-allowed",
              compact ? "h-6 px-1.5 text-2xs" : "h-7 px-2 text-xs",
              active
                ? "bg-primary text-primary-foreground"
                : "text-muted-foreground hover:bg-secondary hover:text-foreground",
            )}
            onClick={() => {
              if (!active) onSelect(level);
            }}
          >
            {level.toUpperCase()}
          </button>
        );
      })}
    </div>
  );
}

/** Inline Drop form prompting for the server-required short reason. */
export function InlineDropForm({
  taskId,
  pending,
  onConfirm,
  onCancel,
}: {
  taskId: string;
  pending?: boolean | undefined;
  onConfirm: (reason: string) => void;
  onCancel: () => void;
}) {
  const [reason, setReason] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const trimmed = reason.trim();

  return (
    <form
      className="relative z-2 flex flex-wrap items-center gap-2 rounded-md border border-error-border bg-error-soft/60 px-2.5 py-2"
      onSubmit={(event) => {
        event.preventDefault();
        if (trimmed && !pending) onConfirm(trimmed);
      }}
      onKeyDown={(event) => {
        if (event.key === "Escape") {
          event.stopPropagation();
          onCancel();
        }
      }}
    >
      <label className="min-w-0 flex-[1_1_180px]">
        <span className="sr-only">Reason to drop {taskId}</span>
        <Input
          ref={inputRef}
          type="text"
          value={reason}
          disabled={pending}
          placeholder="Short reason to drop (required)"
          className="h-7 bg-card text-xs"
          onChange={(event) => setReason(event.currentTarget.value)}
        />
      </label>
      <div className="flex items-center gap-1.5">
        <Button type="submit" variant="destructive" size="xs" disabled={!trimmed || pending}>
          Drop
        </Button>
        <Button type="button" variant="outline" size="xs" disabled={pending} onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}

/** Format a seconds cadence concisely (e.g., `24h`, `6h`, `30m`). */
export function formatCadenceSecs(secs: number): string {
  if (secs % 86_400 === 0) return `${secs / 86_400}d`;
  if (secs % 3600 === 0) return `${secs / 3600}h`;
  if (secs % 60 === 0) return `${secs / 60}m`;
  return `${secs}s`;
}
