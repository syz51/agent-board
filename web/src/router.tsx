// Code-based routes. Search params stay plain strings in the URL (`?ready=1`, not
// `?ready=%221%22`): the router's JSON search encoding is replaced with URLSearchParams.

import {
  createRootRoute,
  createRoute,
  createRouter,
  redirect,
  type RouterHistory,
} from "@tanstack/react-router";
import { Shell, NotFound } from "./app.tsx";
import { AgentsScreen } from "./screens/agents.tsx";
import { AskScreen, AsksScreen } from "./screens/asks.tsx";
import { BoardScreen } from "./screens/board.tsx";
import { HomeScreen } from "./screens/boards.tsx";
import { HealthScreen } from "./screens/health.tsx";
import { RunDetailScreen, RunsScreen } from "./screens/runs.tsx";
import { SchedulesScreen } from "./screens/schedules.tsx";
import { TaskScreen } from "./screens/task.tsx";

/** A search value as a non-empty string, or nothing. */
function text(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}

export type BoardTab = "up-next" | "proposals" | "backlog" | "icebox";

export interface BoardSearch {
  q?: string | undefined;
  priority?: string | undefined;
  /** An agent id, `none` for unowned, or nothing for anyone. */
  owner?: string | undefined;
  ready?: "1" | undefined;
  /** A task id: only its direct children. */
  parent?: string | undefined;
  /** Active operator tier tab (`up-next` when absent). */
  tab?: BoardTab | undefined;
}

export function validateBoardSearch(search: Record<string, unknown>): BoardSearch {
  return {
    q: text(search.q),
    priority: text(search.priority),
    owner: text(search.owner),
    ready: search.ready === "1" ? "1" : undefined,
    parent: text(search.parent),
    tab:
      search.tab === "proposals" || search.tab === "backlog" || search.tab === "icebox"
        ? search.tab
        : undefined,
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

export interface SchedulesSearch {
  host?: string | undefined;
  source?: string | undefined;
  category?: string | undefined;
  status?: string | undefined;
  include_os?: "1" | undefined;
  q?: string | undefined;
  group?: "host" | "category" | "none" | undefined;
  view?: "timeline" | undefined;
  job?: string | undefined;
}

export function validateSchedulesSearch(search: Record<string, unknown>): SchedulesSearch {
  return {
    host: text(search.host),
    source: text(search.source),
    category: text(search.category),
    status: text(search.status),
    include_os: search.include_os === "1" || search.include_os === "true" ? "1" : undefined,
    q: text(search.q),
    group:
      search.group === "host" || search.group === "category" || search.group === "none"
        ? search.group
        : undefined,
    view: search.view === "timeline" ? "timeline" : undefined,
    job: text(search.job),
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
    path: "/health",
    validateSearch: (search: Record<string, unknown>): { days?: string | undefined } => ({
      days: text(search.days),
    }),
    component: HealthScreen,
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/fleet",
    validateSearch: (search: Record<string, unknown>): { days?: string | undefined } => ({
      days: text(search.days),
    }),
    beforeLoad: ({ search }) => {
      throw redirect({ to: "/health", search, replace: true });
    },
  }),
  createRoute({
    getParentRoute: () => rootRoute,
    path: "/schedules",
    validateSearch: validateSchedulesSearch,
    component: SchedulesScreen,
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
