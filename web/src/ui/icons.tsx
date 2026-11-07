// One icon family: 16-unit grid, 1.6 stroke, round caps. Icons are decorative unless a
// caller gives them a label; status and priority always come with text nearby or in `title`.

import type { Priority, TaskStatus } from "../api/schema.ts";

const PATHS = {
  boards: "M2.75 3.25h4.5v4.5h-4.5zM8.75 3.25h4.5v2.5h-4.5zM8.75 7.25h4.5v5.5h-4.5zM2.75 9.25h4.5v3.5h-4.5z",
  inbox: "M2.5 8.75 4 3.5h8l1.5 5.25V12.5h-11zM2.5 8.75h3.25l.75 1.5h3l.75-1.5h3.25",
  agents: "M6 7.5a2.25 2.25 0 1 0 0-4.5 2.25 2.25 0 0 0 0 4.5zM1.75 13c.4-2.4 2.1-4 4.25-4s3.85 1.6 4.25 4M10.5 3.2a2.25 2.25 0 0 1 0 4.1M11.75 9.4c1.3.5 2.2 1.8 2.5 3.6",
  events: "M1.75 8h2.5l2-4.75 3.5 9.5 2-4.75h2.5",
  search: "M7 11.5a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9zM10.25 10.25 13.5 13.5",
  arrow: "M3 8h9.5M9 4.5 12.5 8 9 11.5",
  chevron: "m6 3.5 4.5 4.5L6 12.5",
  check: "m3.5 8.25 3 3 6-6.5",
  question: "M8 13.75a5.75 5.75 0 1 0 0-11.5 5.75 5.75 0 0 0 0 11.5zM6.4 6.4a1.7 1.7 0 1 1 2.3 1.6c-.5.2-.7.6-.7 1.1M8 11.1v.01",
  hand: "M5.5 8.5V3.75a1 1 0 0 1 2 0V7.5M7.5 7V2.75a1 1 0 0 1 2 0V7.25M9.5 7.25V3.75a1 1 0 0 1 2 0V9.5c0 2.5-1.6 4.25-4 4.25-1.6 0-2.6-.7-3.4-1.9L2.6 9.4a1 1 0 0 1 1.6-1.2L5.5 9.75",
  task: "M3.25 2.75h9.5v10.5h-9.5zM5.75 8.1l1.6 1.6 3-3.2",
  agent: "M8 7.25a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5zM3 13.75c.6-2.6 2.6-4.25 5-4.25s4.4 1.65 5 4.25",
  shield: "M8 1.75 13 3.9v3.9c0 3-2.15 5.15-5 6.45-2.85-1.3-5-3.45-5-6.45V3.9zM5.75 8l1.5 1.5 3-3.1",
  handoff: "M2.5 8h10M9 4.5 12.5 8 9 11.5",
  note: "M3.25 3.5h9.5M3.25 6.5h9.5M3.25 9.5h9.5M3.25 12.5h5.5",
  board: "M2.75 2.75h10.5v10.5H2.75zM6.5 2.75v10.5M2.75 6.5H6.5",
  lock: "M3.75 7.25h8.5v6h-8.5zM5.5 7.25V5.25a2.5 2.5 0 0 1 5 0v2",
  file: "M4 1.75h5l3 3v9.5H4zM9 1.75v3h3",
  bolt: "M8.75 1.75 3.5 9h4l-.75 5.25L12 7H8z",
  pause: "M5.75 3.5v9M10.25 3.5v9",
  cloud: "M4.5 12.25a3 3 0 0 1-.35-5.98 4 4 0 0 1 7.7-.77 3.25 3.25 0 0 1 .4 6.75z",
  plug: "M6 1.75v3.5M10 1.75v3.5M4 5.25h8v2.5a4 4 0 0 1-8 0zM8 11.75v2.5",
} as const;

export type IconName = keyof typeof PATHS;

export function Icon({ name, class: className }: { name: IconName; class?: string }) {
  return (
    <svg class={className ? `icon ${className}` : "icon"} viewBox="0 0 16 16" aria-hidden="true">
      <path d={PATHS[name]} />
    </svg>
  );
}

/** The mark beside the product name: three stacked task rows, one of them done. */
export function BrandMark() {
  return (
    <svg class="brand-mark" viewBox="0 0 20 20" aria-hidden="true">
      <rect x="0.5" y="0.5" width="19" height="19" rx="5" />
      <path d="M5 6.5h10M5 10h7M5 13.5h4" />
    </svg>
  );
}

export type StatusKey = TaskStatus | "ready";

const STATUS_TEXT: Record<StatusKey, string> = {
  ready: "Ready",
  open: "Open",
  doing: "Doing",
  blocked: "Blocked",
  review: "Review",
  done: "Done",
  dropped: "Dropped",
};

export function statusLabel(status: StatusKey): string {
  return STATUS_TEXT[status];
}

/**
 * A status as a shape: open is a dashed ring, ready a solid ring, doing half filled,
 * review three quarters, done a filled check, blocked a ring with a bar, dropped a cross.
 */
export function StatusIcon({ status, label }: { status: StatusKey; label?: boolean }) {
  return (
    <svg
      class={`status-icon s-${status}`}
      viewBox="0 0 14 14"
      role={label ? "img" : undefined}
      aria-label={label ? STATUS_TEXT[status] : undefined}
      aria-hidden={label ? undefined : "true"}
    >
      {status === "done"
        ? (
          <>
            <circle cx="7" cy="7" r="6.25" class="fill" />
            <path d="m4.3 7.2 1.9 1.9 3.6-3.8" class="knock" />
          </>
        )
        : (
          <>
            <circle
              cx="7"
              cy="7"
              r="5.6"
              class="ring"
              stroke-dasharray={status === "open" ? "2.2 1.9" : undefined}
            />
            {status === "doing" && <path d="M7 3.4a3.6 3.6 0 0 1 0 7.2z" class="fill" />}
            {status === "review" && <path d="M7 3.4a3.6 3.6 0 1 1-3.6 3.6H7z" class="fill" />}
            {status === "ready" && <circle cx="7" cy="7" r="2" class="fill" />}
            {status === "blocked" && <path d="M4.6 7h4.8" class="bar" />}
            {status === "dropped" && <path d="m5 5 4 4m0-4-4 4" class="bar" />}
          </>
        )}
    </svg>
  );
}

/** A status icon with its name: the pill used wherever status stands alone. */
export function StatusPill({ status }: { status: StatusKey }) {
  return (
    <span class={`pill pill-${status}`}>
      <StatusIcon status={status} />
      {STATUS_TEXT[status]}
    </span>
  );
}

/** Priority as signal bars with the level in text: P1 three bars, P3 one. */
export function PriorityMark({ priority, compact }: { priority: Priority; compact?: boolean }) {
  const level = priority === "p1" ? 3 : priority === "p2" ? 2 : 1;
  return (
    <span class={`prio prio-${priority}${compact ? " prio-compact" : ""}`} title={`Priority ${priority.toUpperCase()}`}>
      <svg viewBox="0 0 12 12" aria-hidden="true">
        {[0, 1, 2].map((bar) => (
          <rect
            key={bar}
            x={1 + bar * 4}
            y={8 - bar * 3}
            width="2.5"
            height={3 + bar * 3}
            rx="0.75"
            class={bar < level ? "on" : "off"}
          />
        ))}
      </svg>
      <span class={compact ? "sr-only" : "prio-text"}>{priority.toUpperCase()}</span>
    </span>
  );
}

/** Two characters that tell agents apart at a glance: `t3:c2d0` → `C2`, `user` → `US`. */
export function monogram(handle: string): string {
  const tail = handle.includes(":") ? handle.slice(handle.indexOf(":") + 1) : handle;
  return (tail.replace(/[^a-z0-9]/gi, "").slice(0, 2) || "?").toUpperCase();
}

/** A neutral monogram beside an agent's handle; the handle itself is always shown too. */
export function Avatar({ handle, large }: { handle: string; large?: boolean }) {
  return <span class={large ? "avatar avatar-lg" : "avatar"} aria-hidden="true">{monogram(handle)}</span>;
}
