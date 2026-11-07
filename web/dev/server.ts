// Dev server: serves the web view against fixture data with a fake event stream, so it
// runs without board-server. Not part of the build.
//
//   bun run dev                     the app on http://localhost:5173, rebuilt on every change
//   bun run preview                 the production build in dist/, same fixtures
//   bun dev/server.ts --events 0    no simulated events (stable screenshots)
//
// Controls, for checking the loading, error and reconnecting states by hand or from a script:
//   /__dev/outage?on=1   every API call answers 500 with an ApiError and live streams drop
//   /__dev/latency?ms=N  delay every API answer by N ms
//   /__dev/emit          produce one simulated event now

import { watch } from "node:fs";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { buildApp } from "../build.ts";
import { DevApiError, DevState } from "./state.ts";

const { values: args } = parseArgs({
  options: {
    port: { type: "string", default: "5173" },
    events: { type: "string", default: "8000" },
    latency: { type: "string", default: "0" },
    dist: { type: "boolean", default: false },
  },
});

const state = new DevState();
let outage = false;
let latencyMs = Number(args.latency);
const streams = new Set<() => void>();

type Handler = (req: Bun.BunRequest, url: URL) => unknown;

/** Wrap a handler: latency, the outage switch, JSON answers and `ApiError` bodies. */
function api(handler: Handler) {
  return async (req: Bun.BunRequest) => {
    if (latencyMs > 0) await Bun.sleep(latencyMs);
    if (outage) {
      return Response.json(
        {
          code: "internal",
          message: "The board database is unavailable (simulated outage).",
          next: "retry in a minute",
        },
        { status: 500 },
      );
    }
    try {
      return Response.json(await handler(req, new URL(req.url)));
    } catch (error) {
      if (error instanceof DevApiError)
        return Response.json(error.body(), { status: error.status });
      throw error;
    }
  };
}

async function body(req: Request): Promise<unknown> {
  try {
    return await req.json();
  } catch {
    throw new DevApiError(400, "invalid", "the body is not JSON", "send a JSON body");
  }
}

function eventStream(req: Request, since: number): Response {
  const encoder = new TextEncoder();
  let close = () => {};
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      const send = (text: string) => controller.enqueue(encoder.encode(text));
      const sendEvent = (event: { seq: number }) =>
        send(`id: ${event.seq}\ndata: ${JSON.stringify(event)}\n\n`);
      send(": connected\n\n");
      for (const event of state.eventsSince(since)) sendEvent(event);
      const unsubscribe = state.onEvent(sendEvent);
      const ping = setInterval(() => send(": ping\n\n"), 15_000);
      close = () => {
        unsubscribe();
        clearInterval(ping);
        streams.delete(close);
        try {
          controller.close();
        } catch {
          // Already closed by the client.
        }
      };
      streams.add(close);
      req.signal.addEventListener("abort", close);
    },
    cancel: () => close(),
  });
  return new Response(stream, {
    headers: { "content-type": "text/event-stream", "cache-control": "no-store" },
  });
}

const notFound = () =>
  Response.json(
    { code: "not_found", message: "no such endpoint", next: "see docs/API.md" },
    { status: 404 },
  );

const root = join(import.meta.dir, "..");
// The dev build goes to a cache directory, rebuilt when a source file changes; reload the
// page to see the change. `--dist` serves the production build as it is.
const distDir = args.dist ? join(root, "dist") : join(root, "node_modules", ".cache", "web-dev");

async function rebuild(): Promise<void> {
  const started = performance.now();
  try {
    await buildApp({ outdir: distDir, production: false });
    console.log(`built in ${Math.round(performance.now() - started)} ms`);
  } catch (error) {
    if (error instanceof AggregateError) for (const log of error.errors) console.error(log);
    console.error(error instanceof Error ? error.message : error);
  }
}

if (!args.dist) {
  await rebuild();
  let pending: ReturnType<typeof setTimeout> | undefined;
  const changed = () => {
    clearTimeout(pending);
    pending = setTimeout(() => void rebuild(), 100);
  };
  watch(join(root, "src"), { recursive: true }, changed);
  watch(join(root, "index.html"), changed);
}

async function serveDist(req: Request): Promise<Response> {
  const path = new URL(req.url).pathname;
  if (path.startsWith("/assets/")) {
    const file = Bun.file(join(distDir, path));
    if (await file.exists()) return new Response(file);
    return new Response("not found", { status: 404 });
  }
  return new Response(Bun.file(join(distDir, "index.html")), {
    headers: { "content-type": "text/html" },
  });
}

const server = Bun.serve({
  port: Number(args.port),
  idleTimeout: 0,
  routes: {
    "/healthz": () =>
      outage
        ? new Response("unavailable", { status: 503 })
        : Response.json({ ok: true, seq: state.seq }),
    "/v1/boards": api(() => state.boards()),
    "/v1/boards/:slug": {
      GET: api((req) => state.board(req.params.slug ?? "")),
      PATCH: api(async (req) => state.setAutopilot(req.params.slug ?? "", await body(req))),
    },
    "/v1/tasks": api((_, url) => state.tasks(url.searchParams)),
    "/v1/tasks/:id": api((req) => state.task(req.params.id ?? "")),
    "/v1/agents": api((_, url) => state.agents(url.searchParams)),
    "/v1/agents/:id/handoff": api((req) => state.handoff(req.params.id ?? "")),
    "/v1/agents/:id/said": api((req, url) => state.said(req.params.id ?? "", url.searchParams)),
    "/v1/updates": api((_, url) => state.updates(url.searchParams)),
    "/v1/updates/seen": { PUT: api(async (req) => state.markSeen(await body(req))) },
    "/v1/asks": api((_, url) => state.asks(url.searchParams)),
    "/v1/asks/:id": api((req) => state.ask(Number(req.params.id))),
    "/v1/asks/:id/answer": {
      POST: api(async (req) => state.answerAsk(Number(req.params.id), await body(req))),
    },
    "/v1/events": async (req) => {
      const url = new URL(req.url);
      const since = Number(
        req.headers.get("last-event-id") ?? url.searchParams.get("since") ?? state.seq,
      );
      if (req.headers.get("accept")?.includes("text/event-stream")) {
        if (outage) return new Response("unavailable", { status: 503 });
        return eventStream(req, since);
      }
      return api(() =>
        state.events(
          since,
          Number(url.searchParams.get("limit") ?? 100),
          url.searchParams.get("board"),
        ),
      )(req);
    },
    "/v1/*": notFound,
    "/__dev/outage": (req) => {
      outage = new URL(req.url).searchParams.get("on") !== "0";
      if (outage) for (const close of [...streams]) close();
      return Response.json({ outage });
    },
    "/__dev/latency": (req) => {
      latencyMs = Number(new URL(req.url).searchParams.get("ms") ?? 0);
      return Response.json({ latencyMs });
    },
    "/__dev/emit": () => Response.json(state.simulate()),
    "/*": serveDist,
  },
});

const every = Number(args.events);
if (every > 0) setInterval(() => state.simulate(), every);

console.log(
  `agent-board web (${args.dist ? "dist" : "dev"}) on ${server.url}` +
    (every > 0 ? `, a simulated event every ${every / 1000}s` : ", no simulated events"),
);
