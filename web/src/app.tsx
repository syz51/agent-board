import { Link, Outlet, useRouter, useRouterState } from "@tanstack/react-router";
import { useEffect, useRef } from "react";
import { cn } from "@/lib/utils.ts";
import { useConnection, useNow } from "@/live/runtime.ts";
import type { ConnectionState } from "@/live/stream.ts";
import { useNeedsYouCount } from "@/screens/asks.tsx";
import { unseenCount, useUpdates } from "@/screens/updates.tsx";
import { Empty, useTitle } from "@/ui/common.tsx";
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
    <div className="min-h-screen bg-background">
      <a
        className="fixed -top-12 left-2 z-30 rounded-sm bg-card px-3 py-2 focus:top-2 focus:shadow-overlay"
        href="#main"
      >
        Skip to content
      </a>
      {/* A two-tier header: the product and connection, then the sections as underlined tabs. */}
      <header className="sticky top-0 z-10 border-b bg-sidebar">
        <div className="mx-auto flex h-14 max-w-[1280px] items-center gap-4 px-4 max-md:h-12 md:px-8">
          <Link
            className="inline-flex min-h-8 items-center gap-2 text-base font-semibold whitespace-nowrap text-foreground hover:no-underline"
            to="/"
          >
            <BrandMark />
            <span>agent-board</span>
          </Link>
          <span className="ml-auto">
            <ConnectionPill />
          </span>
        </div>
        <div className="mx-auto max-w-[1280px] px-2 md:px-6">
          <Nav />
        </div>
      </header>
      <ConnectionBanner />
      <main
        id="main"
        ref={main}
        tabIndex={-1}
        className="mx-auto max-w-[1280px] px-4 pt-6 pb-16 md:px-8 lg:pt-8"
      >
        <Outlet />
      </main>
    </div>
  );
}

function usePathname(): string {
  return useRouterState({ select: (state) => state.location.pathname });
}

const NAV: {
  to: "/" | "/asks" | "/agents" | "/events";
  label: string;
  icon: IconName;
  match: (path: string) => boolean;
}[] = [
  {
    to: "/",
    label: "Boards",
    icon: "boards",
    match: (path) => path === "/" || /^\/(boards|tasks)\//.test(path),
  },
  { to: "/asks", label: "Needs you", icon: "inbox", match: (path) => path.startsWith("/asks") },
  { to: "/agents", label: "Agents", icon: "agents", match: (path) => path.startsWith("/agents") },
  { to: "/events", label: "Events", icon: "events", match: (path) => path.startsWith("/events") },
];

export function Nav() {
  const path = usePathname();
  // Open asks plus the tasks waiting on the user without one, as the page lists them.
  const open = useNeedsYouCount();
  const updates = useUpdates();
  const unseen = updates.data ? unseenCount(updates.data) : 0;
  return (
    <nav className="-mb-px flex gap-1 overflow-x-auto [scrollbar-width:none]" aria-label="Main">
      {NAV.map((item) => (
        <Link
          key={item.to}
          to={item.to}
          // Boards stays current on a board and a task; the router's own match would not.
          aria-current={item.match(path) ? "page" : undefined}
          className="group/tab relative flex h-12 flex-none items-center border-b-2 border-transparent px-2 text-sm text-foreground hover:no-underline aria-[current=page]:border-tab-current aria-[current=page]:font-semibold max-md:h-11"
        >
          <span className="flex items-center gap-2 rounded-md px-2 py-1.5 group-hover/tab:bg-secondary">
            <Icon name={item.icon} className="text-muted-foreground" />
            <span>{item.label}</span>
            {item.to === "/asks" && open > 0 && (
              <span
                className="h-5 min-w-5 rounded-full bg-secondary px-1.5 text-center text-xs leading-5 font-semibold text-foreground tabular-nums"
                aria-label={`${open} waiting on you`}
              >
                {open}
              </span>
            )}
            {item.to === "/asks" && unseen > 0 && (
              <>
                {/* One mark at a time: the count when something waits, else a dot for news. */}
                {open === 0 && (
                  <span
                    className="size-2 flex-none rounded-full bg-link"
                    aria-hidden="true"
                    title={`${unseen} unseen updates`}
                  />
                )}
                <span className="sr-only">
                  , {unseen} unseen {unseen === 1 ? "update" : "updates"}
                </span>
              </>
            )}
          </span>
        </Link>
      ))}
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
        "inline-flex items-center gap-1.75 text-meta whitespace-nowrap",
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
        "flex items-center justify-center gap-2.5 border-b px-4 py-2 text-meta",
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
      <Link to="/">Go to boards</Link>
    </Empty>
  );
}
