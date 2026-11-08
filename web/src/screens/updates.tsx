// "Since you last looked" on the home page: tasks done and coordinators' turn endings
// since the user's seen cursor (`GET /v1/updates`). A coordinator's closing message can be
// followed at once by a wake-up that starts its next turn, so this is where the user reads it.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { type ReactNode, useId, useMemo, useState } from "react";
import { type Api, api, UPDATES_MAX } from "@/api/client.ts";
import type { Agent, DoneUpdate, Said, Updates } from "@/api/schema.ts";
import { Alert, AlertDescription } from "@/components/ui/alert.tsx";
import { Badge } from "@/components/ui/badge.tsx";
import { Button } from "@/components/ui/button.tsx";
import { cn } from "@/lib/utils.ts";
import { fetching, invalidateTags, queries } from "@/live/query.ts";
import { tags } from "@/live/refresh.ts";
import { Markdown, PlainText, SafeLink, safeHref } from "@/text/markdown.tsx";
import {
  agentIndex,
  Empty,
  ErrorNotice,
  listClass,
  QueryFrame,
  RelTime,
  useAgents,
  Who,
} from "@/ui/common.tsx";
import { Icon } from "@/ui/icons.tsx";

/** Updates since the seen cursor, for one board or all; shared with the navigation dot. */
export function useUpdates(board = "") {
  return useQuery(queries.updates(board));
}

export function unseenCount(updates: Updates): number {
  return updates.done.length + updates.said.length;
}

/** The time of the newest update listed, as the server wrote it; null when none is. */
export function newestAt(updates: Updates): string | null {
  let newest: string | null = null;
  for (const at of [
    ...updates.done.map((item) => item.done_at),
    ...updates.said.map((item) => item.at),
  ]) {
    if (newest === null || Date.parse(at) > Date.parse(newest)) newest = at;
  }
  return newest;
}

/**
 * Mark everything shown as seen: the cursor moves to the newest shown update, not to now,
 * so updates that arrived after the list loaded stay unseen.
 */
export async function markShownSeen(
  client: Pick<Api, "markSeen">,
  updates: Updates,
): Promise<void> {
  const at = newestAt(updates);
  if (at !== null) await client.markSeen(at);
}

/** The updates on every board, for the home page's section. */
export function Updates() {
  const updates = useUpdates();
  const agentList = useAgents().data;
  const agents = useMemo(() => agentIndex(agentList), [agentList]);
  return (
    <QueryFrame query={updates} label="Loading updates">
      {(data) => <UpdatesView updates={data} agents={agents} />}
    </QueryFrame>
  );
}

/** The lists themselves; exported for tests. */
export function UpdatesView({ updates, agents }: { updates: Updates; agents: Map<string, Agent> }) {
  const total = unseenCount(updates);
  return (
    <>
      {updates.more && (
        <Alert variant="warn" className="mb-3 px-3.5 py-2.5">
          <AlertDescription className="text-warn">
            Only the newest {UPDATES_MAX} of each kind are listed; older ones were cut. Marking all
            seen covers them too. Each task's page keeps its full history.
          </AlertDescription>
        </Alert>
      )}
      {total === 0 ? (
        <Empty title="You're up to date">
          Finished tasks and what coordinators said at the end of their turns appear here until you
          mark them seen.
        </Empty>
      ) : (
        <>
          {updates.done.length > 0 && (
            <UpdateGroup title="Finished tasks" count={updates.done.length}>
              {updates.done.map((item) => (
                <DoneRow key={item.task} item={item} agents={agents} />
              ))}
            </UpdateGroup>
          )}
          {updates.said.length > 0 && (
            <UpdateGroup title="Coordinators said" count={updates.said.length}>
              {updates.said.map((item) => (
                <SaidRow key={item.id} item={item} agents={agents} />
              ))}
            </UpdateGroup>
          )}
        </>
      )}
    </>
  );
}

/**
 * The section's header aside: since when updates are listed, and a quiet button to mark
 * them all seen. Exported for tests.
 */
export function UpdatesHeader({
  updates,
  client = api,
}: {
  updates: Updates;
  client?: Pick<Api, "markSeen">;
}) {
  return (
    <>
      <span className="text-meta whitespace-nowrap text-muted-foreground">
        {updates.since ? (
          <>
            Since <RelTime at={updates.since} />
          </>
        ) : (
          "Everything kept"
        )}
      </span>
      {unseenCount(updates) > 0 && <MarkSeen updates={updates} client={client} />}
    </>
  );
}

function MarkSeen({ updates, client }: { updates: Updates; client: Pick<Api, "markSeen"> }) {
  const queryClient = useQueryClient();
  const mutation = useMutation({
    mutationFn: fetching(() => markShownSeen(client, updates)),
    onSuccess: () => invalidateTags(queryClient, [tags.updates]),
  });
  return (
    <>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        className="text-meta"
        onClick={() => mutation.mutate()}
        disabled={mutation.isPending}
      >
        <Icon name="check" className="size-3.5" />
        {mutation.isPending ? "Marking…" : "Mark all seen"}
      </Button>
      {mutation.error && <ErrorNotice error={mutation.error} compact />}
    </>
  );
}

function UpdateGroup({
  title,
  count,
  children,
}: {
  title: string;
  count: number;
  children: ReactNode;
}) {
  return (
    <div className="mt-4 first:mt-0" data-slot="update-group">
      <h3 className="mb-1.5 flex items-baseline gap-1.5 text-meta font-semibold text-subtle-foreground">
        {title} <span className="font-medium text-muted-foreground tabular-nums">{count}</span>
      </h3>
      <ul className={listClass}>{children}</ul>
    </div>
  );
}

const updateMeta = "text-meta text-muted-foreground";

/** PRs and commits: what the user opens to see the change. */
const SHOWN_EVIDENCE = new Set(["pr", "commit"]);

/**
 * A finished task on one line, like a task row; its summary and evidence open below it on
 * demand, so the list stays as dense as the rest of the page.
 */
function DoneRow({ item, agents }: { item: DoneUpdate; agents: Map<string, Agent> }) {
  const [open, setOpen] = useState(false);
  const detailId = useId();
  const evidence = item.evidence.filter((entry) => SHOWN_EVIDENCE.has(entry.kind));
  return (
    <li>
      <div className="grid min-h-9 grid-cols-[24px_minmax(0,1fr)_auto] items-center gap-x-2 py-1 pr-(--gutter) pl-[calc(var(--gutter)-6px)] max-md:grid-cols-[24px_minmax(0,1fr)] max-md:gap-y-0.5">
        <button
          type="button"
          className="grid size-6 place-items-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground focus-visible:outline-2 focus-visible:outline-ring"
          aria-expanded={open}
          aria-controls={detailId}
          aria-label={`${open ? "Hide" : "Show"} the summary of ${item.task}`}
          onClick={() => setOpen(!open)}
        >
          <Icon
            name="chevronDown"
            className={cn(
              "size-3.5 transition-transform motion-reduce:transition-none",
              !open && "-rotate-90",
            )}
          />
        </button>
        <h4 className="min-w-0 font-medium">
          <Link
            className="block truncate py-[3.5px] text-foreground"
            to="/tasks/$id"
            params={{ id: item.task }}
          >
            <span className="ident mr-1.5 text-muted-foreground">{item.task}</span>
            <PlainText text={item.title} />
          </Link>
        </h4>
        <p className={cn(updateMeta, "whitespace-nowrap max-md:col-start-2")}>
          <Link
            className="inline-flex min-h-6 items-center"
            to="/boards/$slug"
            params={{ slug: item.board }}
          >
            {item.board}
          </Link>
          {item.owner && (
            <>
              {" · "}
              <Who id={item.owner} agents={agents} />
            </>
          )}
          {" · done "}
          <RelTime at={item.done_at} />
        </p>
      </div>
      <div
        id={detailId}
        hidden={!open}
        className="flex flex-col gap-1.5 pt-0.5 pr-(--gutter) pb-3 pl-[calc(var(--gutter)+26px)]"
      >
        {item.parent && (
          <p className={updateMeta}>
            {"Part of "}
            <Link to="/tasks/$id" params={{ id: item.parent }}>
              <span className="ident">{item.parent}</span>
            </Link>
          </p>
        )}
        {item.summary ? (
          <div className="max-w-[72ch]">
            <Markdown text={item.summary} />
          </div>
        ) : (
          <p className="text-muted-foreground">No summary written.</p>
        )}
        {evidence.length > 0 && (
          <ul className="flex flex-wrap gap-x-4.5 gap-y-1.5" aria-label="Evidence">
            {evidence.map((entry, index) => (
              <li key={index} className="flex min-w-0 items-center gap-1.5">
                <Badge variant="outline" shape="tag">
                  {entry.kind === "pr" ? "PR" : "commit"}
                </Badge>
                <span className="min-w-0 [overflow-wrap:anywhere]">
                  {safeHref(entry.value) ? (
                    <SafeLink href={entry.value}>{entry.value}</SafeLink>
                  ) : (
                    <code>{entry.value}</code>
                  )}
                </span>
              </li>
            ))}
          </ul>
        )}
      </div>
    </li>
  );
}

/** A coordinator's closing message: who and where on one line, the text below it. */
function SaidRow({ item, agents }: { item: Said; agents: Map<string, Agent> }) {
  return (
    <li className="flex min-w-0 flex-col gap-1 px-(--gutter) py-2.5">
      <p className={cn(updateMeta, "flex flex-wrap items-center gap-x-1")}>
        <Who id={item.agent} agents={agents} />
        {" on "}
        <Link
          className="inline-flex min-h-6 items-center"
          to="/boards/$slug"
          params={{ slug: item.board }}
        >
          {item.board}
        </Link>
        {" · "}
        <RelTime at={item.at} />
      </p>
      <CollapsibleMarkdown text={item.text} />
    </li>
  );
}

/** Longer than this, or than `CLAMP_LINES` lines, and agent text starts collapsed. */
const CLAMP_CHARS = 360;
const CLAMP_LINES = 6;

export function isLong(text: string): boolean {
  return text.length > CLAMP_CHARS || text.split("\n").length > CLAMP_LINES;
}

/** Agent markdown, collapsed to a few lines when long, with a button to show all of it. */
export function CollapsibleMarkdown({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  if (!isLong(text)) {
    return (
      <div className="mt-0.5 max-w-[72ch]">
        <Markdown text={text} />
      </div>
    );
  }
  return (
    <div className="flex flex-col items-start">
      <div
        id={id}
        className={cn("mt-0.5 max-w-[72ch]", !open && "clamped")}
        data-clamped={open ? undefined : ""}
      >
        <Markdown text={text} />
      </div>
      <Button
        type="button"
        variant="link"
        className="min-h-9 px-0 py-1.5 text-sm underline"
        aria-expanded={open}
        aria-controls={id}
        onClick={() => setOpen(!open)}
      >
        {open ? "Show less" : "Show all"}
      </Button>
    </div>
  );
}
