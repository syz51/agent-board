// Sections the user arranges. Each one collapses; on the home page they also move up or
// down, by buttons or by dragging the grip. The arrangement is kept per browser in
// localStorage. Where storage is unavailable (a private window, blocked site data) the
// defaults show, and changes last until the page reloads.

import { type DragEvent, Fragment, type ReactNode, useId, useState } from "react";
import { cn } from "@/lib/utils.ts";
import { Icon } from "./icons.tsx";

const PREFIX = "agent-board:";

function readList(key: string): string[] | undefined {
  let value: unknown;
  try {
    const raw = localStorage.getItem(PREFIX + key);
    value = raw === null ? undefined : JSON.parse(raw);
  } catch {
    // Unreadable or blocked storage: fall back to the defaults.
    return undefined;
  }
  return Array.isArray(value) && value.every((item) => typeof item === "string")
    ? value
    : undefined;
}

function writeList(key: string, value: readonly string[]): void {
  try {
    localStorage.setItem(PREFIX + key, JSON.stringify(value));
  } catch {
    // Blocked or full storage: the change still applies until the page reloads.
  }
}

/** The collapsed ids kept under `key`, `initial` until the user changes one. */
export function useCollapsed(
  key: string,
  initial: readonly string[] = [],
): [ReadonlySet<string>, (id: string) => void] {
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(
    () => new Set(readList(key) ?? initial),
  );
  const toggle = (id: string) => {
    const next = new Set(collapsed);
    if (!next.delete(id)) next.add(id);
    writeList(key, [...next]);
    setCollapsed(next);
  };
  return [collapsed, toggle];
}

/**
 * The saved order of the ids still known, then the ids not saved yet in their default
 * order, so a new section appears and a removed one drops out. Exported for tests.
 */
export function arrange(saved: readonly string[] | undefined, ids: readonly string[]): string[] {
  const kept = [...new Set(saved ?? [])].filter((id) => ids.includes(id));
  return [...kept, ...ids.filter((id) => !kept.includes(id))];
}

/** `order` with `id` moved to `index`, clamped to the list. Exported for tests. */
export function moveTo(order: readonly string[], id: string, index: number): string[] {
  const rest = order.filter((item) => item !== id);
  if (rest.length === order.length) return [...order];
  const at = Math.max(0, Math.min(index, rest.length));
  return [...rest.slice(0, at), id, ...rest.slice(at)];
}

/** The order of `ids` kept under `key`, and a function that moves one to an index. */
export function useOrder(
  key: string,
  ids: readonly string[],
): [string[], (id: string, index: number) => void] {
  const [saved, setSaved] = useState(() => readList(key));
  const order = arrange(saved, ids);
  const move = (id: string, index: number) => {
    const next = moveTo(order, id, index);
    writeList(key, next);
    setSaved(next);
  };
  return [order, move];
}

const DRAG_TYPE = "application/x-agent-board-section";

export interface SectionPlace {
  index: number;
  count: number;
  move: (index: number) => void;
  /** The section being dragged over this one, if any; set by the list. */
  dropTarget: boolean;
  onDragState: (state: "start" | "over" | "end") => void;
}

/**
 * A titled section whose heading collapses it. With `place`, it also moves: up and down
 * buttons for keyboard and touch, and a grip to drag it on a wide screen.
 */
export function Section({
  title,
  count,
  icon,
  aside,
  collapsed,
  onToggle,
  place,
  children,
}: {
  title: string;
  count?: number | undefined;
  icon?: ReactNode;
  /** Links or actions beside the heading; hidden while collapsed. */
  aside?: ReactNode;
  collapsed: boolean;
  onToggle: () => void;
  place?: SectionPlace | undefined;
  children: ReactNode;
}) {
  const bodyId = useId();
  const titleId = useId();
  const drop = place && {
    onDragOver: (event: DragEvent) => {
      if (!event.dataTransfer.types.includes(DRAG_TYPE)) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "move";
      place.onDragState("over");
    },
    onDrop: (event: DragEvent) => {
      if (!event.dataTransfer.types.includes(DRAG_TYPE)) return;
      event.preventDefault();
      place.move(place.index);
    },
  };
  return (
    <section
      aria-labelledby={titleId}
      className={cn(
        "group/section rounded-lg not-last:mb-6",
        place?.dropTarget && "outline-2 outline-offset-4 outline-ring/60",
      )}
      {...drop}
    >
      <div className="mb-2 flex min-h-8 items-center gap-2">
        {place && (
          <span
            className="hidden w-4 flex-none cursor-grab items-center justify-center text-subtle-foreground opacity-0 group-hover/section:opacity-100 active:cursor-grabbing lg:-ml-5 lg:flex"
            draggable
            onDragStart={(event) => {
              event.dataTransfer.setData(DRAG_TYPE, title);
              event.dataTransfer.effectAllowed = "move";
              place.onDragState("start");
            }}
            onDragEnd={() => place.onDragState("end")}
            title="Drag to move this section"
            aria-hidden="true"
          >
            <Icon name="grip" className="size-3.5 stroke-[2.6]" />
          </span>
        )}
        <h2 id={titleId} className="flex min-w-0 text-sm font-semibold">
          <button
            type="button"
            className="-mx-1.5 inline-flex min-h-8 min-w-0 items-center gap-2 rounded-md px-1.5 text-left hover:bg-muted focus-visible:outline-2 focus-visible:outline-ring"
            aria-expanded={!collapsed}
            aria-controls={bodyId}
            onClick={onToggle}
          >
            <Icon
              name="chevronDown"
              className={cn(
                "size-3.5 flex-none text-muted-foreground transition-transform motion-reduce:transition-none",
                collapsed && "-rotate-90",
              )}
            />
            {icon}
            <span className="truncate">{title}</span>
            {count !== undefined && (
              <span className="text-meta font-medium text-muted-foreground tabular-nums">
                {count}
              </span>
            )}
          </button>
        </h2>
        {/* Beside the title, so the space they keep while hidden does not push the aside in. */}
        {place && <MoveButtons title={title} place={place} />}
        <div className="ml-auto flex items-center gap-1.5">{!collapsed && aside}</div>
      </div>
      <div id={bodyId} hidden={collapsed}>
        {children}
      </div>
    </section>
  );
}

/** Up and down: shown on hover or focus where a pointer hovers, always on touch screens. */
function MoveButtons({ title, place }: { title: string; place: SectionPlace }) {
  const button =
    "grid size-8 place-items-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring disabled:pointer-events-none disabled:opacity-35";
  return (
    <div className="flex items-center opacity-100 [@media(hover:hover)]:opacity-0 [@media(hover:hover)]:group-focus-within/section:opacity-100 [@media(hover:hover)]:group-hover/section:opacity-100">
      <button
        type="button"
        className={button}
        aria-label={`Move ${title} up`}
        disabled={place.index === 0}
        onClick={() => place.move(place.index - 1)}
      >
        <Icon name="chevronDown" className="size-3.5 rotate-180" />
      </button>
      <button
        type="button"
        className={button}
        aria-label={`Move ${title} down`}
        disabled={place.index === place.count - 1}
        onClick={() => place.move(place.index + 1)}
      >
        <Icon name="chevronDown" className="size-3.5" />
      </button>
    </div>
  );
}

/**
 * Sections the user orders and collapses, kept under `storageKey`. Each item renders
 * through `render` with its collapsed state and place.
 */
export function ArrangedSections<Id extends string>({
  storageKey,
  ids,
  render,
}: {
  storageKey: string;
  ids: readonly Id[];
  render: (
    id: Id,
    state: { collapsed: boolean; onToggle: () => void; place: SectionPlace },
  ) => ReactNode;
}) {
  const [order, move] = useOrder(`${storageKey}:order`, ids);
  const [collapsed, toggle] = useCollapsed(`${storageKey}:collapsed`);
  const [dragging, setDragging] = useState<string | null>(null);
  const [over, setOver] = useState<string | null>(null);
  return (
    <>
      {(order as Id[]).map((id, index) => (
        <Fragment key={id}>
          {render(id, {
            collapsed: collapsed.has(id),
            onToggle: () => toggle(id),
            place: {
              index,
              count: order.length,
              move: (to) => {
                // A drop moves the dragged section here; the buttons move this one.
                move(dragging ?? id, to);
                setDragging(null);
                setOver(null);
              },
              dropTarget: dragging !== null && dragging !== id && over === id,
              onDragState: (state) => {
                if (state === "start") setDragging(id);
                else if (state === "over") setOver(id);
                else {
                  setDragging(null);
                  setOver(null);
                }
              },
            },
          })}
        </Fragment>
      ))}
    </>
  );
}
