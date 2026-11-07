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
import { api } from "./api/client.ts";
import { tags } from "./live/refresh.ts";
import { useResource } from "./live/runtime.ts";
import { Empty, useTitle } from "./ui/common.tsx";
import { BrandMark, Icon, type IconName } from "./ui/icons.tsx";

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
    <div class="app">
      <a class="skip" href="#main">Skip to content</a>
      <header class="sidebar">
        <a class="brand" href="/">
          <BrandMark />
          <span>agent-board</span>
        </a>
        <Nav />
        <BoardLinks />
        <ConnectionPill />
      </header>
      <div class="main-col">
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
      </div>
    </div>
  );
}

const NAV: { href: string; label: string; icon: IconName; match: (path: string) => boolean }[] = [
  { href: "/", label: "Boards", icon: "boards", match: (path: string) => path === "/" || /^\/(boards|tasks)\//.test(path) },
  { href: "/asks", label: "Needs you", icon: "inbox", match: (path: string) => path.startsWith("/asks") },
  { href: "/agents", label: "Agents", icon: "agents", match: (path: string) => path.startsWith("/agents") },
  { href: "/events", label: "Events", icon: "events", match: (path: string) => path.startsWith("/events") },
];

function Nav() {
  const { path } = useLocation();
  const asks = useOpenAsks();
  const open = asks.data?.length ?? 0;
  return (
    <nav class="nav" aria-label="Main">
      {NAV.map((item) => (
        <a key={item.href} href={item.href} aria-current={item.match(path) ? "page" : undefined}>
          <Icon name={item.icon} />
          <span class="nav-label">{item.label}</span>
          {item.href === "/asks" && open > 0 && (
            <span class="nav-count" aria-label={`${open} open`}>{open}</span>
          )}
        </a>
      ))}
    </nav>
  );
}

/** Each board, with its open asks, so a board is one click from anywhere on a wide screen. */
function BoardLinks() {
  const { path } = useLocation();
  const boards = useResource("boards", [tags.boards], api.boards);
  const list = (boards.data ?? []).filter((board) => board.status !== "closed");
  return (
    <nav class="side-boards" aria-label="Boards">
      <p class="side-label" aria-hidden="true">Boards</p>
      {boards.data === undefined && (
        <p class="side-empty">{boards.error ? "Boards are unavailable" : "Loading…"}</p>
      )}
      <ul>
        {list.map((board) => {
          const href = `/boards/${encodeURIComponent(board.slug)}`;
          const here = path === href || path.startsWith(`/tasks/${board.slug}-`);
          return (
            <li key={board.slug}>
              <a href={href} aria-current={here ? "page" : undefined}>
                <span class="side-board-mark" aria-hidden="true">{board.title.trim()[0]?.toUpperCase() ?? "#"}</span>
                <span class="side-board-title">{board.title}</span>
                {board.open_asks > 0 && (
                  <span class="side-count" aria-label={`${board.open_asks} open asks`}>{board.open_asks}</span>
                )}
              </a>
            </li>
          );
        })}
      </ul>
    </nav>
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
      <Icon name="plug" />
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
