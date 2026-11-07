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
 * The app frame: one centred column under a quiet top bar with the sections as a segmented
 * pill, and bottom tabs on phones (below 720px). Focus moves to the page on each navigation so
 * screen readers announce it.
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
        className="fixed -top-12 left-2 z-30 rounded-sm bg-popover px-3 py-2 focus:top-2 focus:shadow-overlay"
        href="#main"
      >
        Skip to content
      </a>
      <header className="sticky top-0 z-10 border-b bg-background">
        <div className="mx-auto flex h-14 max-w-[944px] items-center gap-6 px-4 max-md:justify-between md:px-8">
          <Link
            className="inline-flex min-h-8 items-center gap-2.25 font-serif text-[19px] font-semibold whitespace-nowrap text-foreground hover:no-underline"
            to="/"
          >
            <BrandMark />
            <span>agent-board</span>
          </Link>
          <Nav />
          <ConnectionPill />
        </div>
      </header>
      <div className="min-w-0">
        <ConnectionBanner />
        <main
          id="main"
          ref={main}
          tabIndex={-1}
          className="mx-auto max-w-[944px] px-4 pt-7 pb-16 max-md:pb-[calc(92px+env(safe-area-inset-bottom))] md:px-8 md:pt-10 md:pb-[88px]"
        >
          <Outlet />
        </main>
      </div>
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
    <nav
      className="flex gap-0.5 max-md:fixed max-md:inset-x-0 max-md:bottom-0 max-md:z-10 max-md:h-[calc(60px+env(safe-area-inset-bottom))] max-md:gap-0 max-md:border-t max-md:bg-background max-md:px-1 max-md:pb-[env(safe-area-inset-bottom)] md:mx-auto md:rounded-full md:bg-secondary md:p-1"
      aria-label="Main"
    >
      {NAV.map((item) => (
        <Link
          key={item.to}
          to={item.to}
          // Boards stays current on a board and a task; the router's own match would not.
          aria-current={item.match(path) ? "page" : undefined}
          className="relative flex min-h-8 items-center gap-2 rounded-full px-3.5 py-1 text-meta font-medium text-subtle-foreground hover:text-foreground hover:no-underline max-md:flex-1 max-md:flex-col max-md:justify-center max-md:gap-0.75 max-md:rounded-none max-md:px-0.5 max-md:py-1.5 max-md:text-2xs max-md:tracking-normal max-md:hover:bg-transparent max-md:aria-[current=page]:bg-transparent max-md:aria-[current=page]:font-medium max-md:aria-[current=page]:text-link md:aria-[current=page]:bg-popover md:aria-[current=page]:text-foreground md:aria-[current=page]:shadow-[0_1px_2px_rgb(0_0_0/0.08),0_0_0_1px_var(--border)] max-md:[&>svg]:size-5.25 md:[&>svg]:hidden"
        >
          <Icon name={item.icon} />
          <span>{item.label}</span>
          {item.to === "/asks" && open > 0 && (
            <span
              className="ml-0.5 h-4.5 min-w-5 rounded-full bg-amber px-1.5 text-center text-2xs leading-4.5 font-[650] tracking-normal text-background tabular-nums max-md:absolute max-md:top-1 max-md:left-[calc(50%+6px)] max-md:m-0"
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
                  className="ml-0.5 size-2 flex-none rounded-full bg-primary max-md:absolute max-md:top-1.5 max-md:left-[calc(50%+8px)] max-md:m-0"
                  aria-hidden="true"
                  title={`${unseen} unseen updates`}
                />
              )}
              <span className="sr-only">
                , {unseen} unseen {unseen === 1 ? "update" : "updates"}
              </span>
            </>
          )}
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
