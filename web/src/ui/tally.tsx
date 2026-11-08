import type { Board, TaskStatus } from "@/api/schema.ts";
import { cn } from "@/lib/utils.ts";

/** Tally segments in bar order: finished work first. Dropped tasks only get a count. */
const SEGMENTS = [
  { key: "done", label: "done", bar: "bg-status-done" },
  { key: "review", label: "review", bar: "bg-status-review" },
  { key: "doing", label: "doing", bar: "bg-status-doing" },
  { key: "blocked", label: "blocked", bar: "bg-status-blocked" },
  { key: "ready", label: "ready", bar: "bg-status-ready" },
  { key: "waiting", label: "waiting", bar: "bg-input" },
  { key: "dropped", label: "dropped", bar: null },
] as const;

export type TallyCounts = Record<(typeof SEGMENTS)[number]["key"], number>;

/** A board's counts as a tally; the board tally leaves dropped tasks out. */
export function boardTally(counts: Board["counts"]): TallyCounts {
  return {
    done: counts.done,
    review: counts.review,
    doing: counts.doing,
    blocked: counts.blocked,
    ready: counts.ready,
    waiting: Math.max(0, counts.open - counts.ready),
    dropped: 0,
  };
}

/** Tasks counted by status, open split into ready and waiting. Exported for tests. */
export function taskTally(tasks: { status: TaskStatus; ready: boolean }[]): TallyCounts {
  const counts: TallyCounts = {
    done: 0,
    review: 0,
    doing: 0,
    blocked: 0,
    ready: 0,
    waiting: 0,
    dropped: 0,
  };
  for (const task of tasks) {
    counts[task.status === "open" ? (task.ready ? "ready" : "waiting") : task.status] += 1;
  }
  return counts;
}

/**
 * Tasks by status: a bar split in proportion, then each count after a dot of its bar
 * colour. The counts carry the meaning; the bar is hidden from assistive technology.
 */
export function StatusTally({
  counts,
  label,
  readyAuto,
  empty = "No tasks yet",
  className,
}: {
  counts: TallyCounts;
  /** What shows when every count is zero. */
  empty?: string;
  /** Names the list of counts, such as "Tasks by status". */
  label: string;
  /** How many ready tasks the coordinator may start; shown after ready when known. */
  readyAuto?: number | null;
  className?: string;
}) {
  const total = SEGMENTS.reduce((sum, segment) => sum + counts[segment.key], 0);
  return (
    <div className={cn("grid gap-1.5", className)}>
      <div
        className="flex h-1.5 gap-0.5 overflow-hidden rounded-[3px] bg-secondary"
        aria-hidden="true"
      >
        {SEGMENTS.map(
          (segment) =>
            segment.bar &&
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
        <p className="text-meta leading-[18px] text-muted-foreground">{empty}</p>
      ) : (
        <ul
          // Phones set it a step smaller and tighter, so six counts still fit on one line.
          className="flex flex-wrap gap-x-2 gap-y-0.5 text-meta leading-[18px] text-muted-foreground max-md:gap-x-1 max-md:text-xs"
          aria-label={label}
        >
          {SEGMENTS.map(
            (segment) =>
              counts[segment.key] > 0 && (
                // A count and its word, in the bar's order, after a dot of its bar colour.
                <li
                  key={segment.key}
                  className="inline-flex items-center gap-1 whitespace-nowrap max-md:gap-0.75"
                >
                  <span
                    className={cn(
                      "size-1.5 flex-none rounded-full",
                      segment.bar ?? "border border-input",
                    )}
                    aria-hidden="true"
                  />
                  <strong className="font-semibold text-foreground tabular-nums">
                    {counts[segment.key]}
                  </strong>{" "}
                  <span>{segment.label}</span>
                  {segment.key === "ready" && readyAuto !== undefined && readyAuto !== null && (
                    <>
                      {" "}
                      <span>(auto {readyAuto})</span>
                    </>
                  )}
                </li>
              ),
          )}
        </ul>
      )}
    </div>
  );
}
