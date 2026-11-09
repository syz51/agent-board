// T3 tells agents to cite a thread as `t3-thread://v2/<environment>/<thread id>`. No browser
// opens that scheme, and T3 Code does not register it with the OS, so the web view maps it
// to T3's https thread page. The https base comes from a registered agent's `t3_link`
// (`<origin>/threads/<environment>/<encoded thread id>`) for the same environment.

import { createContext } from "react";
import type { Agent } from "@/api/schema.ts";

/** T3 thread-page bases, such as `https://t3.example/threads/`, by environment id. */
export type ThreadBases = ReadonlyMap<string, string>;

/** The bases for links in rendered text; empty, so nothing maps, until the agents load. */
export const ThreadBasesContext = createContext<ThreadBases>(new Map());

/** An environment id: T3 uses UUIDs, which need no escaping in a URL path. */
const ENVIRONMENT = /^[A-Za-z0-9_-]+$/;

/** The thread page path of a `t3_link`: `…/threads/<environment>/<thread>`. */
const THREAD_PATH = /^(.*\/threads\/)([^/]+)\/[^/]+$/;

/** Each environment's thread-page base, read from the https `t3_link` agents registered. */
export function threadBases(agents: readonly Agent[] | undefined): ThreadBases {
  const bases = new Map<string, string>();
  for (const agent of agents ?? []) {
    const link = agent.native_ids.t3_link;
    if (!link) continue;
    let url: URL;
    try {
      url = new URL(link);
    } catch {
      continue;
    }
    const match = THREAD_PATH.exec(url.pathname);
    if (url.protocol !== "https:" || !match) continue;
    const [, prefix, environment] = match;
    if (!prefix || !environment || !ENVIRONMENT.test(environment)) continue;
    if (!bases.has(environment)) bases.set(environment, url.origin + prefix);
  }
  return bases;
}

const T3_THREAD = /^t3-thread:\/\/v2\/([^/?#\s]+)\/([^/?#\s]+)$/;

/** A launch id (`mcp:launch:…`) names a pending launch, not a thread T3 can show. */
const LAUNCH = /(^|:)launch:/;

/**
 * The https URL of the thread a `t3-thread://v2/<environment>/<thread id>` link names, or
 * null when the link has another shape, names a launch, or no agent gave its environment's
 * base.
 */
export function threadUrl(href: string, bases: ThreadBases): string | null {
  const match = T3_THREAD.exec(href.trim());
  if (!match) return null;
  const [, environment, thread] = match;
  if (!environment || !thread || !ENVIRONMENT.test(environment) || LAUNCH.test(thread)) {
    return null;
  }
  const base = bases.get(environment);
  // The id is used as written: T3's own links percent-encode it once more, `%` included.
  return base ? `${base}${environment}/${encodeURIComponent(thread)}` : null;
}
