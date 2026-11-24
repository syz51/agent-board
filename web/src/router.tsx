// Code-based routes. Search params stay plain strings in the URL (`?ready=1`, not
// `?ready=%221%22`): the router's JSON search encoding is replaced with URLSearchParams.

import {
  createRootRoute,
  createRoute,
  createRouter,
  type RouterHistory,
} from "@tanstack/react-router";
import { Shell, NotFound } from "./app.tsx";
import { AgentsScreen } from "./screens/agents.tsx";
import { AskScreen, AsksScreen } from "./screens/asks.tsx";
import { BoardScreen } from "./screens/board.tsx";
import { HomeScreen } from "./screens/boards.tsx";
import { FleetScreen } from "./screens/fleet.tsx";
import { RunDetailScreen, RunsScreen } from "./screens/runs.tsx";
import { TaskScreen } from "./screens/task.tsx";

/** A search value as a non-empty string, or nothing. */
function text(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}

export interface BoardSearch {
  q?: string | undefined;
  priority?: string | undefined;
  /** An agent id, `none` for unowned, or nothing for anyone. */
  owner?: string | undefined;
  ready?: "1" | undefined;
  /** A task id: only its direct children. */
  parent?: string | undefined;
}

export function validateBoardSearch(search: Record<string, unknown>): BoardSearch {
  return {
    q: text(search.q),
    priority: text(search.priority),
    owner: text(search.owner),
    ready: search.ready === "1" ? "1" : undefined,
    parent: text(search.parent),
  };
}

export interface RunsSearch {
  active?: "1" | undefined;
  origin?: string | undefined;
  kind?: string | undefined;
  device?: string | undefined;
  board?: string | undefined;
  state?: string | undefined;
}

export function validateRunsSearch(search: Record<string, unknown>): RunsSearch {
  return {
    active: search.active === "1" ? "1" : undefined,
    origin: text(search.origin),
    kind: text(search.kind),
    device: text(search.device),
    board: text(search.board),
    state: text(search.state),
  };
}

export function parseSearch(search: string): Record<string, string> {
  return Object.fromEntries(new URLSearchParams(search));
}

/** Search params in order, without empty ones. */
export function stringifySearch(search: Record<string, unknown>): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(search)) {
    if (
      (typeof value === "string" && value !== "") ||
      typeof value === "number" ||
      typeof value === "boolean"
    ) {
      params.set(key, String(value));
    }
  }
  const query = params.toString();
  return query ? `?${query}` : "";
}

const rootRoute = createRootRoute({ component: Shell, notFoundComponent: NotFound });

const routeTree = rootRoute.addChildren([
  createRoute({ getParentRoute: () => rootRoute, path: "/", component: HomeScreen }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/boards/$slug",
    validateSearch: validateBoardSearch,
    component: BoardScreen,
  }),
  createRoute({ getParentRoute: () => rootRoute, path: "/tasks/$id", component: TaskScreen }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/agents",
    validateSearch: (search: Record<string, unknown>): { state?: string | undefined } => ({
      state: text(search.state),
    }),
    component: AgentsScreen,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/asks",
    validateSearch: (search: Record<string, unknown>): { board?: string | undefined } => ({
      board: text(search.board),
    }),
    component: AsksScreen,
  }),
  createRoute({ getParentRoute: () => rootRoute, path: "/asks/$id", component: AskScreen }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/runs",
    validateSearch: validateRunsSearch,
    component: RunsScreen,
  }),
  createRoute({ getParentRoute: () => rootRoute, path: "/runs/$id", component: RunDetailScreen }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/fleet",
    validateSearch: (search: Record<string, unknown>): { days?: string | undefined } => ({
      days: text(search.days),
    }),
    component: FleetScreen,
  }),
]);

export function createAppRouter(history?: RouterHistory) {
  return createRouter({
    routeTree,
    parseSearch,
    stringifySearch,
    scrollRestoration: true,
    defaultPreload: false,
    ...(history ? { history } : {}),
  });
}

declare module "@tanstack/react-router" {
  interface Register {
    router: ReturnType<typeof createAppRouter>;
  }
}
