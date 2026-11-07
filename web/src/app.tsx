import { LocationProvider, Route, Router, useLocation } from "preact-iso";
import { useRef } from "preact/hooks";
import type { ConnectionState } from "./live/stream.ts";
import { useConnection, useNow } from "./live/runtime.ts";
import { AgentsScreen } from "./screens/agents.tsx";
import { AskScreen, AsksScreen, useOpenAsks } from "./screens/asks.tsx";
import { BoardScreen } from "./screens/board.tsx";
import { BoardsScreen } from "./screens/boards.tsx";
import { EventsScreen } from "./screens/events.tsx";
import { TaskScreen } from "./screens/task.tsx";
import { unseenCount, useUpdates } from "./screens/updates.tsx";
import { Empty, useTitle } from "./ui/common.tsx";

export function App() {
  return (
    <LocationProvider>
      <Shell />
    </LocationProvider>
  );
}

function Shell() {
  const main = useRef<HTMLElement>(null);
  return (
    <>
      <a class="skip" href="#main">Skip to content</a>
      <header class="topbar">
        <a class="brand" href="/">agent-board</a>
        <Nav />
        <ConnectionPill />
      </header>
      <ConnectionBanner />
      <main id="main" ref={main} tabIndex={-1}>
        <Router
          onRouteChange={() => {
            // Move focus to the new page so screen readers announce it.
            main.current?.focus({ preventScroll: true });
          }}
        >
          <Route path="/" component={BoardsScreen} />
          <Route path="/boards/:slug" component={BoardScreen} />
          <Route path="/tasks/:id" component={TaskScreen} />
          <Route path="/agents" component={AgentsScreen} />
          <Route path="/asks" component={AsksScreen} />
          <Route path="/asks/:id" component={AskScreen} />
          <Route path="/events" component={EventsScreen} />
          <Route default component={NotFound} />
        </Router>
      </main>
    </>
  );
}

const NAV = [
  { href: "/", label: "Boards", icon: "boards", match: (path: string) => path === "/" || /^\/(boards|tasks)\//.test(path) },
  { href: "/asks", label: "Needs you", icon: "asks", match: (path: string) => path.startsWith("/asks") },
  { href: "/agents", label: "Agents", icon: "agents", match: (path: string) => path.startsWith("/agents") },
  { href: "/events", label: "Events", icon: "events", match: (path: string) => path.startsWith("/events") },
] as const;

function Nav() {
  const { path } = useLocation();
  const asks = useOpenAsks();
  const open = asks.data?.length ?? 0;
  const updates = useUpdates();
  const unseen = updates.data ? unseenCount(updates.data) : 0;
  return (
    <nav class="nav" aria-label="Main">
      {NAV.map((item) => (
        <a key={item.href} href={item.href} aria-current={item.match(path) ? "page" : undefined}>
          <Icon name={item.icon} />
          <span class="nav-label">{item.label}</span>
          {item.icon === "asks" && open > 0 && (
            <span class="nav-count" aria-label={`${open} open`}>{open}</span>
          )}
          {item.icon === "asks" && unseen > 0 && (
            <>
              <span class="nav-dot" aria-hidden="true" title={`${unseen} unseen updates`} />
              <span class="visually-hidden">, {unseen} unseen {unseen === 1 ? "update" : "updates"}</span>
            </>
          )}
        </a>
      ))}
    </nav>
  );
}

const ICONS = {
  boards: "M4 4h7v7H4zM13 4h7v4h-7zM13 10h7v10h-7zM4 13h7v7H4z",
  asks: "M4 13l2.5-8h11l2.5 8v6H4zM4 13h5l1 2h4l1-2h5",
  agents: "M9 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7zM2.5 20c.5-3.5 3.2-6 6.5-6s6 2.5 6.5 6M16 4.3a3.5 3.5 0 0 1 0 6.4M18 14.5c2 .8 3.2 2.8 3.5 5.5",
  events: "M3 12h4l3-7 4 14 3-7h4",
} as const;

function Icon({ name }: { name: keyof typeof ICONS }) {
  return (
    <svg class="icon" viewBox="0 0 24 24" aria-hidden="true">
      <path d={ICONS[name]} />
    </svg>
  );
}

function connectionText(state: ConnectionState, now: number): string {
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
    <span class={`conn conn-${state.kind}`} role="status">
      <span class="conn-dot" aria-hidden="true" />
      <span class="conn-text">{connectionText(state, now)}</span>
    </span>
  );
}

function ConnectionBanner() {
  const state = useConnection();
  if (state.kind === "live" || state.kind === "connecting") return null;
  return (
    <div class={`banner banner-${state.kind}`} role="status">
      {state.kind === "offline"
        ? "You're offline. What you see may be out of date; it refreshes when the connection returns."
        : "Lost the connection to the board server. Reconnecting; what you see may be out of date."}
    </div>
  );
}

function NotFound() {
  useTitle("Not found");
  return (
    <Empty title="This page doesn't exist">
      <a href="/">Go to boards</a>
    </Empty>
  );
}
