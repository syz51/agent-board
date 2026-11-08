// Render a component inside the app's providers: a query client and a router on a memory
// history, so links resolve and mutations run.
import { QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { act, render } from "@testing-library/react";
import type { QueryClient } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { createQueryClient } from "../src/live/query.ts";
import { createAppRouter, parseSearch, stringifySearch } from "../src/router.tsx";

export async function renderInApp(
  node: ReactNode,
  { path = "/", seed }: { path?: string; seed?: (client: QueryClient) => void } = {},
) {
  const client = createQueryClient();
  seed?.(client);
  const router = createRouter({
    routeTree: createRootRoute({ component: () => node }),
    history: createMemoryHistory({ initialEntries: [path] }),
    parseSearch,
    stringifySearch,
  });
  await act(() => router.load());
  const result = render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { ...result, client, router };
}

/**
 * Render the whole app (the shell and the route's screen) at `path`, as the page does, with
 * the app's own routes: for what depends on the route, its params and the shell around it.
 */
export async function renderRoute(
  path: string,
  { seed }: { seed?: (client: QueryClient) => void } = {},
) {
  const client = createQueryClient();
  seed?.(client);
  const router = createAppRouter(createMemoryHistory({ initialEntries: [path] }));
  await act(() => router.load());
  const result = render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { ...result, client, router };
}
