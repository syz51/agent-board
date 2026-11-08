import { useQuery } from "@tanstack/react-query";
import { Link, Outlet, useRouter, useRouterState } from "@tanstack/react-router";
import { useEffect, useRef } from "react";
import { cn } from "@/lib/utils.ts";
import { queries } from "@/live/query.ts";
import { useConnection, useNow } from "@/live/runtime.ts";
import type { ConnectionState } from "@/live/stream.ts";
import { useOpenAsks } from "@/screens/asks.tsx";
import { Count, Empty, useTitle } from "@/ui/common.tsx";
import { BrandMark, Icon, type IconName } from "@/ui/icons.tsx";

/**
 * The app frame: a sidebar from 900px, a top bar below, and bottom tabs on phones (below
 * 720px). Focus moves to the page on each navigation so screen readers announce it.
 */
export function Shell() {
  const main = useRef<HTMLElement>(null);
  const router = useRouter();
  useEffect(
    () =>
      router.subscribe("onResolved", (event) => {
        if (event.fromLocation && event.pathChanged) main.current?.focus({ preventScroll: true });
      }),
    [router],
  );
  return (
    <div className="min-h-screen bg-background lg:grid lg:grid-cols-[224px_minmax(0,1fr)] lg:bg-sidebar">
      <a
        className="fixed -top-12 left-2 z-30 rounded-sm bg-card px-3 py-2 focus:top-2 focus:shadow-overlay"
        href="#main"
      >
        Skip to content
      </a>
      <header className="sticky top-0 z-10 flex h-12 items-center gap-5 border-b bg-sidebar px-4 max-md:justify-between lg:h-screen lg:flex-col lg:items-stretch lg:gap-4 lg:border-b-0 lg:px-2.5 lg:py-3">
        <Link
          className="inline-flex min-h-8 items-center gap-2 text-sm font-semibold whitespace-nowrap text-foreground hover:no-underline lg:px-2"
          to="/"
        >
          <BrandMark />
          <span>agent-board</span>
        </Link>
        <Nav />
        <BoardLinks />
        <ConnectionPill />
      </header>
      {/* The content sits on an inset panel, raised off the sidebar's tone on wide screens. */}
      <div className="min-w-0 lg:py-3 lg:pr-3">
        <div className="bg-background [--gutter:--spacing(4)] lg:min-h-[calc(100vh-24px)] lg:overflow-clip lg:rounded-lg lg:shadow-panel lg:[--gutter:--spacing(7)]">
          <ConnectionBanner />
          <main
            id="main"
            ref={main}
            tabIndex={-1}
            className="px-(--gutter) pt-5 pb-16 max-md:pb-[calc(92px+env(safe-area-inset-bottom))] lg:pt-6"
          >
            <Outlet />
          </main>
        </div>
      </div>
    </div>
  );
}

function usePathname(): string {
  return useRouterState({ select: (state) => state.location.pathname });
}

/**
 * Whether a navigation item is the current page ("page"), holds it ("true": a board or a
 * task is under Home), or neither.
 */
type Current = "page" | "true" | undefined;

const NAV: {
  to: "/" | "/asks" | "/agents";
  label: string;
  icon: IconName;
  match: (path: string) => Current;
}[] = [
  {
    to: "/",
    label: "Home",
    icon: "boards",
    match: (path) =>
      path === "/" ? "page" : /^\/(boards|tasks)\//.test(path) ? "true" : undefined,
  },
  {
    to: "/asks",
    label: "Needs you",
    icon: "inbox",
    match: (path) => (path.startsWith("/asks") ? "page" : undefined),
  },
  {
    to: "/agents",
    label: "Agents",
    icon: "agents",
    match: (path) => (path.startsWith("/agents") ? "page" : undefined),
  },
];

/** A count's place on a nav item: at the end on wide screens, over the icon on phones. */
const NAV_COUNT =
  "ml-auto max-md:absolute max-md:top-1 max-md:left-[calc(50%+6px)] max-md:m-0 max-md:h-4 max-md:min-w-4 max-md:px-1 max-md:text-2xs";

export function Nav() {
  const path = usePathname();
  const asks = useOpenAsks();
  const open = asks.data?.length ?? 0;
  // Without the asks, the count is unknown, not zero: an offline mark says so.
  const unknown = asks.data === undefined && asks.error !== null;
  return (
    <nav
      className="flex flex-1 gap-0.5 max-md:fixed max-md:inset-x-0 max-md:bottom-0 max-md:z-10 max-md:h-[calc(60px+env(safe-area-inset-bottom))] max-md:gap-0 max-md:border-t max-md:bg-card max-md:px-1 max-md:pb-[env(safe-area-inset-bottom)] lg:flex-none lg:flex-col"
      aria-label="Main"
    >
      {NAV.map((item) => (
        <Link
          key={item.to}
          to={item.to}
          // On a board or a task, Home holds the page: wide screens mark the board in the
          // list below instead, and phones, which have no board list, mark the Home tab.
          aria-current={item.match(path)}
          className="relative flex min-h-7.5 items-center gap-2.25 rounded-md px-2 py-1 font-medium text-sidebar-foreground hover:bg-sidebar-accent hover:text-foreground hover:no-underline aria-[current=page]:bg-sidebar-accent aria-[current=page]:font-semibold aria-[current=page]:text-foreground max-lg:aria-[current=true]:bg-sidebar-accent max-lg:aria-[current=true]:font-semibold max-lg:aria-[current=true]:text-foreground max-md:flex-1 max-md:flex-col max-md:justify-center max-md:gap-0.75 max-md:rounded-none max-md:px-0.5 max-md:py-1.5 max-md:text-2xs max-md:tracking-normal max-md:hover:bg-transparent max-md:aria-[current=page]:bg-transparent max-md:aria-[current=page]:font-medium max-md:aria-[current=page]:text-link max-md:aria-[current=true]:bg-transparent max-md:aria-[current=true]:font-medium max-md:aria-[current=true]:text-link max-md:[&>svg]:size-5.25"
        >
          <Icon name={item.icon} />
          <span>{item.label}</span>
          {item.to === "/asks" && open > 0 && (
            <Count
              value={open}
              tone="solid"
              className={NAV_COUNT}
              aria-label={`${open} waiting on you`}
            />
          )}
          {item.to === "/asks" && unknown && (
            <span
              className={cn(
                NAV_COUNT,
                "grid place-items-center text-muted-foreground max-md:bg-card",
              )}
              title="Unknown until the connection is back"
              role="img"
              aria-label="Count unknown until the connection is back"
            >
              <Icon name="plug" className="size-3.5" />
            </span>
          )}
        </Link>
      ))}
    </nav>
  );
}

/** Each board, with its open asks, so a board is one click from anywhere on a wide screen. */
function BoardLinks() {
  const path = usePathname();
  const boards = useQuery(queries.boards());
  const list = (boards.data ?? []).filter((board) => board.status !== "closed");
  return (
    <nav className="hidden min-h-0 flex-1 overflow-y-auto lg:block" aria-label="Boards">
      <p className="px-2 pb-1 text-xs font-medium text-muted-foreground" aria-hidden="true">
        Boards
      </p>
      {boards.data === undefined &&
        (boards.error ? (
          // Placeholder rows where the boards go, so the list reads as not loaded, not empty.
          <>
            <ul className="grid gap-px" aria-hidden="true">
              {[64, 88, 72].map((width) => (
                <li key={width} className="flex min-h-7.5 items-center gap-2.25 px-2 py-1">
                  <span className="size-4.5 flex-none rounded-[5px] bg-sidebar-accent" />
                  <span className="h-2 rounded-full bg-sidebar-accent" style={{ width }} />
                </li>
              ))}
            </ul>
            <p className="px-2 pt-1 text-meta text-muted-foreground">
              Boards show again once the connection is back.
            </p>
          </>
        ) : (
          <p className="px-2.5 py-1 text-meta text-muted-foreground">Loading…</p>
        ))}
      <ul className="grid gap-px">
        {list.map((board) => {
          const href = `/boards/${encodeURIComponent(board.slug)}`;
          const here = path === href || path.startsWith(`/tasks/${board.slug}-`);
          return (
            <li key={board.slug}>
              <Link
                to="/boards/$slug"
                params={{ slug: board.slug }}
                aria-current={here ? "page" : undefined}
                className="flex min-h-7.5 items-center gap-2.25 rounded-md px-2 py-1 text-sidebar-foreground hover:bg-sidebar-accent hover:no-underline aria-[current=page]:bg-sidebar-accent aria-[current=page]:font-[550] aria-[current=page]:text-foreground"
              >
                <span
                  className="grid size-4.5 flex-none place-items-center rounded-[5px] border border-input bg-card text-2xs font-semibold tracking-normal text-subtle-foreground"
                  aria-hidden="true"
                >
                  {board.title.trim()[0]?.toUpperCase() ?? "#"}
                </span>
                <span className="min-w-0 flex-1 truncate">{board.title}</span>
                {board.open_asks > 0 && (
                  <>
                    <Count value={board.open_asks} tone="soft" aria-hidden="true" />
                    <span className="sr-only">
                      , {board.open_asks} {board.open_asks === 1 ? "open ask" : "open asks"}
                    </span>
                  </>
                )}
              </Link>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}

export function connectionText(state: ConnectionState, now: number): string {
  switch (state.kind) {
    case "live":
      return "Live";
    case "connecting":
      return "Connecting";
    case "offline":
      return "Offline";
    case "reconnecting": {
      const seconds = Math.max(0, Math.ceil((state.retryAt - now) / 1000));
      return seconds > 0 ? `Retrying in ${seconds}s` : "Reconnecting";
    }
  }
}

function ConnectionPill() {
  const state = useConnection();
  const now = useNow(1000);
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1.75 text-meta whitespace-nowrap lg:mt-auto lg:border-t lg:px-2.5 lg:pt-2",
        state.kind === "offline" || state.kind === "reconnecting"
          ? "text-foreground"
          : "text-muted-foreground",
      )}
      role="status"
      data-state={state.kind}
    >
      <span
        className={cn(
          "size-1.75 rounded-full",
          state.kind === "live" &&
            "bg-status-ready shadow-[0_0_0_3px_color-mix(in_srgb,var(--status-ready)_20%,transparent)]",
          (state.kind === "connecting" || state.kind === "reconnecting") && "bg-amber",
          state.kind === "offline" && "bg-status-blocked",
        )}
        aria-hidden="true"
      />
      <span>{connectionText(state, now)}</span>
    </span>
  );
}

function ConnectionBanner() {
  const state = useConnection();
  if (state.kind === "live" || state.kind === "connecting") return null;
  return (
    <div
      className={cn(
        "flex items-center gap-2.5 border-b px-(--gutter) py-2 text-meta",
        state.kind === "offline"
          ? "border-error-border bg-error-soft text-error"
          : "border-warn-border bg-warn-soft text-warn",
      )}
      role="status"
    >
      <Icon name="plug" />
      {state.kind === "offline"
        ? "You're offline. What you see may be out of date; it refreshes when the connection returns."
        : "Lost the connection to the board server. Reconnecting; what you see may be out of date."}
    </div>
  );
}

export function NotFound() {
  useTitle("Not found");
  return (
    <Empty title="This page doesn't exist">
      <Link to="/">Go home</Link>
    </Empty>
  );
}
