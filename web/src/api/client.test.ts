import { describe, expect, test } from "bun:test";
import { DevState } from "../../dev/state.ts";
import { ApiFailure, createApi, type Fetch } from "./client.ts";

interface Call {
  url: string;
  init: RequestInit | undefined;
}

/** A fetch that answers every call with `respond` and records what was asked. */
function fakeFetch(respond: (url: string, init?: RequestInit) => Response | Promise<Response>) {
  const calls: Call[] = [];
  const fetchImpl: Fetch = async (url, init) => {
    calls.push({ url, init });
    return respond(url, init);
  };
  return { api: createApi(fetchImpl), calls };
}

const json = (body: unknown, status = 200) => Response.json(body, { status });

async function failure(promise: Promise<unknown>): Promise<ApiFailure> {
  try {
    await promise;
  } catch (error) {
    if (error instanceof ApiFailure) return error;
    throw error;
  }
  throw new Error("expected the call to fail");
}

describe("api client parsing", () => {
  const state = new DevState(() => Date.parse("2026-11-06T12:00:00Z"));

  test("decodes every read the web view makes from fixture answers", async () => {
    const { api } = fakeFetch((url) => {
      const { pathname, searchParams } = new URL(url, "http://board.test");
      const handoff = pathname.match(/^\/v1\/agents\/(.+)\/handoff$/);
      if (handoff) return json(state.handoff(decodeURIComponent(handoff[1] ?? "")));
      const said = pathname.match(/^\/v1\/agents\/(.+)\/said$/);
      if (said) return json(state.said(decodeURIComponent(said[1] ?? ""), searchParams));
      if (pathname === "/v1/updates") return json(state.updates(searchParams));
      if (pathname === "/healthz") return json({ ok: true, seq: state.seq });
      if (pathname === "/v1/boards") return json(state.boards());
      if (pathname.startsWith("/v1/boards/")) return json(state.board(pathname.slice(11)));
      if (pathname === "/v1/tasks") return json(state.tasks(searchParams));
      if (pathname.startsWith("/v1/tasks/"))
        return json(state.task(decodeURIComponent(pathname.slice(10))));
      if (pathname === "/v1/agents") return json(state.agents(searchParams));
      if (pathname === "/v1/asks") return json(state.asks(searchParams));
      if (pathname.startsWith("/v1/asks/")) return json(state.ask(Number(pathname.slice(9))));
      if (pathname === "/v1/events")
        return json(state.events(Number(searchParams.get("since")), 100, null));
      return json({ code: "not_found", message: url, next: null }, 404);
    });

    expect((await api.health()).seq).toBe(state.seq);
    const boards = await api.boards();
    expect(boards.map((board) => board.slug)).toEqual([
      "factory",
      "web-view",
      "docs",
      "release-engineering",
      "sandbox",
    ]);
    expect(boards[0]?.counts.ready).toBeGreaterThan(0);
    expect((await api.board("factory")).autopilot).toBe(true);
    expect((await api.tasks({ board: "factory" })).length).toBeGreaterThan(5);
    const detail = await api.task("factory-12");
    expect(detail.task.blockers).toEqual(["factory-9"]);
    expect(detail.notes.length).toBe(2);
    expect(detail.history.every((event) => event.task === "factory-12")).toBe(true);
    const agents = await api.agents();
    expect(agents.find((agent) => agent.state === "paused")?.paused_until).toEqual(
      expect.any(String),
    );
    expect(await api.handoff(agents[0]?.id ?? "")).toMatchObject({ next: "run the live suite" });
    expect((await api.asks({ status: "open" })).every((ask) => ask.status === "open")).toBe(true);
    expect((await api.ask(5)).answered_via).toBe("telegram");
    expect((await api.events({ since: 0 })).events.length).toBeGreaterThan(0);
    expect((await api.task("factory-9")).task.summary).toContain("CRLF");
    expect((await api.task("factory-12")).task.summary).toBeNull();

    const updates = await api.updates();
    expect(updates.since).toBe(updates.seen_at);
    expect(updates.done.map((item) => item.task)).toEqual(["factory-9", "web-view-5", "docs-7"]);
    expect(updates.done[1]?.evidence.map((item) => item.kind)).toEqual(["pr", "commit", "check"]);
    expect(updates.done[2]).toMatchObject({ parent: "docs-1", summary: null });
    expect(updates.said.map((item) => item.id)).toEqual([5, 3, 4]);
    expect(updates.more).toBe(false);
    expect((await api.updates({ board: "docs" })).done.map((item) => item.task)).toEqual([
      "docs-7",
    ]);
    const everything = await api.updates({ since: "2020-01-01T00:00:00Z" });
    expect(everything.said.length).toBe(5);

    const coordinator = agents.find((agent) => agent.handle === "t3:c2d0")?.id ?? "";
    const said = await api.agentSaid(coordinator, 3);
    expect(said.map((item) => item.id)).toEqual([5, 4, 2]);
    expect(said.every((item) => item.agent === coordinator)).toBe(true);
  });

  test("decodes updates, a seen cursor and turn endings, and rejects bad ones", async () => {
    const said = {
      id: 3,
      agent: "t3:c2d0",
      board: "factory",
      text: "Round done.",
      at: "2026-11-06T12:00:00Z",
    };
    const done = {
      task: "factory-9",
      board: "factory",
      parent: null,
      title: "Fix CRLF",
      summary: "Fixed.",
      owner: "t3:a91c",
      done_at: "2026-11-06T11:00:00Z",
      evidence: [
        {
          kind: "pr" as const,
          value: "https://github.com/example/factory/pull/41",
          exit_code: null,
          agent: "t3:a91c",
          at: "2026-11-06T11:00:00Z",
        },
      ],
    };
    const body = { seen_at: null, since: null, done: [done], said: [said], more: true };
    expect(await fakeFetch(() => json(body)).api.updates()).toEqual(body);
    expect(
      await fakeFetch(() => json({ seen_at: "2026-11-06T12:00:00Z" })).api.markSeen(
        "2026-11-06T12:00:00Z",
      ),
    ).toEqual({ seen_at: "2026-11-06T12:00:00Z" });
    expect(await fakeFetch(() => json([said])).api.agentSaid("t3:c2d0")).toEqual([said]);

    const { summary: _, owner: __, parent: ___, ...bare } = done;
    const decoded = await fakeFetch(() => json({ ...body, done: [bare] })).api.updates();
    expect(decoded.done[0]).toMatchObject({ summary: null, owner: null, parent: null });

    const badSaid = fakeFetch(() => json({ ...body, said: [{ ...said, id: "3" }] }));
    expect((await failure(badSaid.api.updates())).message).toContain(
      'said[0].id: expected an integer, got "3"',
    );
    const badTime = fakeFetch(() => json({ ...body, done: [{ ...done, done_at: "earlier" }] }));
    expect((await failure(badTime.api.updates())).message).toContain(
      "done[0].done_at: expected an RFC 3339 time",
    );
    const noMore = fakeFetch(() => json({ seen_at: null, since: null, done: [], said: [] }));
    expect((await failure(noMore.api.updates())).message).toContain("more: expected a boolean");
    const badCursor = fakeFetch(() => json({ seen_at: null }));
    expect((await failure(badCursor.api.markSeen("2026-11-06T12:00:00Z"))).code).toBe(
      "bad_response",
    );
  });

  test("builds same-origin URLs, encodes path segments and drops empty params", async () => {
    const { api, calls } = fakeFetch(() => json([]));
    await api.tasks({ board: "web view/x", status: "" });
    await api.tasks({ status: "doing", limit: 5 });
    await api.asks({ status: "open" });
    await api.handoff("t3:7f3a").catch(() => {});
    await api.updates().catch(() => {});
    await api.updates({ board: "factory", since: "2026-11-06T12:00:00Z" }).catch(() => {});
    await api.agentSaid("t3:c2d0", 3).catch(() => {});
    await api.agentSaid("t3:c2d0").catch(() => {});
    expect(calls.map((call) => call.url)).toEqual([
      "/v1/tasks?board=web+view%2Fx&limit=1000",
      "/v1/tasks?status=doing&limit=5",
      "/v1/asks?status=open",
      "/v1/agents/t3%3A7f3a/handoff",
      "/v1/updates",
      "/v1/updates?board=factory&since=2026-11-06T12%3A00%3A00Z",
      "/v1/agents/t3%3Ac2d0/said?limit=3",
      "/v1/agents/t3%3Ac2d0/said",
    ]);
    expect(calls[0]?.init?.credentials).toBe("same-origin");
    // No identity or auth headers: the Tailscale header is added by `tailscale serve`.
    expect(Object.keys(calls[0]?.init?.headers ?? {})).toEqual(["accept"]);
  });

  test("sends JSON bodies for writes", async () => {
    const { api, calls } = fakeFetch((_, init) => {
      const body = JSON.parse(String(init?.body)) as { autopilot?: boolean; answer?: string };
      if (body.autopilot !== undefined) return json(state.board("factory"));
      return json(state.ask(5));
    });
    await api.setAutopilot("factory", false);
    await api.answerAsk(7, "The existing ops chat");
    await api.markSeen("2026-11-06T11:58:00Z").catch(() => {});
    expect(calls.map((call) => [call.init?.method, call.url, call.init?.body])).toEqual([
      ["PATCH", "/v1/boards/factory", '{"autopilot":false}'],
      ["POST", "/v1/asks/7/answer", '{"answer":"The existing ops chat"}'],
      ["PUT", "/v1/updates/seen", '{"at":"2026-11-06T11:58:00Z"}'],
    ]);
    expect((calls[1]?.init?.headers as Record<string, string>)["content-type"]).toBe(
      "application/json",
    );
  });

  test("turns an ApiError body into a failure with its message and next step", async () => {
    const { api } = fakeFetch(() =>
      json(
        {
          code: "claim_refused",
          message: "t3:c2d0 owns factory-12",
          next: "pick another task from ready",
        },
        409,
      ),
    );
    const error = await failure(api.task("factory-12"));
    expect(error).toMatchObject({
      status: 409,
      code: "claim_refused",
      message: "t3:c2d0 owns factory-12",
      next: "pick another task from ready",
    });
  });

  test("accepts error codes this version does not know", async () => {
    const { api } = fakeFetch(() =>
      json({ code: "rate_limited", message: "slow down", next: null }, 429),
    );
    expect(await failure(api.boards())).toMatchObject({ code: "rate_limited", next: null });
  });

  test("describes an error that is not an ApiError, such as a proxy page", async () => {
    const { api } = fakeFetch(
      () => new Response("<html>Bad gateway</html>", { status: 502, statusText: "Bad Gateway" }),
    );
    const error = await failure(api.boards());
    expect(error.code).toBe("http_502");
    expect(error.message).toBe("The server answered 502 Bad Gateway.");
    expect(error.next).toContain("try again");
  });

  test("reports an unreachable server as a network failure", async () => {
    const { api } = fakeFetch(() => Promise.reject(new TypeError("Failed to fetch")));
    const error = await failure(api.boards());
    expect(error).toMatchObject({ status: 0, code: "network" });
    expect(error.next).toContain("tailnet");
  });

  test("rejects a body that is not JSON", async () => {
    const { api } = fakeFetch(() => new Response("<!doctype html>", { status: 200 }));
    const error = await failure(api.boards());
    expect(error.code).toBe("bad_response");
    expect(error.message).toContain("not JSON");
  });

  test("rejects a response of the wrong shape and names the field", async () => {
    const board = state.board("factory");
    const { api } = fakeFetch(() => json([{ ...board, counts: { ...board.counts, ready: "3" } }]));
    const error = await failure(api.boards());
    expect(error.code).toBe("bad_response");
    expect(error.message).toContain('[0].counts.ready: expected an integer, got "3"');
  });

  test("rejects unknown enum values and bad timestamps", async () => {
    const task = state.task("factory-12").task;
    const bad = fakeFetch(() => json([{ ...task, status: "archived" }]));
    expect((await failure(bad.api.tasks({}))).message).toContain(
      "[0].status: expected one of open, doing",
    );
    const badTime = fakeFetch(() => json([{ ...task, updated_at: "yesterday" }]));
    expect((await failure(badTime.api.tasks({}))).message).toContain(
      "[0].updated_at: expected an RFC 3339 time",
    );
  });

  test("ignores fields it does not know and accepts absent optional fields", async () => {
    const { blocked_reason: _, ...task } = state.task("factory-12").task;
    const { api } = fakeFetch(() => json([{ ...task, new_field: 1 }]));
    const [decoded] = await api.tasks({});
    expect(decoded?.blocked_reason).toBeNull();
    expect(decoded).not.toHaveProperty("new_field");
  });

  test("reads fixed and resolution kinds it does not know", async () => {
    const task = state.task("factory-12").task;
    const item = (kind: string) => ({
      ...task,
      feedback: {
        about: "board",
        source: "agent",
        signature: "board:x",
        reports: 1,
        reporters: ["t3:a"],
        last_reported_at: "2026-11-06T12:00:00Z",
        resolution: { kind, reason: "changed x", at: "2026-11-06T12:00:00Z", by: "t3:a" },
      },
    });
    const { api } = fakeFetch(() => json([item("fixed"), item("future_kind")]));
    const kinds = (await api.tasks({})).map((t) => t.feedback?.resolution?.kind);
    expect(kinds).toEqual(["fixed", "unknown"]);
  });

  test("keeps unknown event types for the feed", async () => {
    const { api } = fakeFetch(() =>
      json({
        events: [
          {
            seq: 4,
            at: "2026-11-06T12:00:00Z",
            board: null,
            actor: "server",
            type: "future_thing",
            task: null,
            payload: { a: 1 },
          },
        ],
        next_since: 4,
      }),
    );
    expect((await api.events({ since: 3 })).events[0]?.type).toBe("future_thing");
  });

  test("a missing handoff is null, other handoff errors still fail", async () => {
    const missing = fakeFetch(() =>
      json({ code: "not_found", message: "no handoff", next: null }, 404),
    );
    expect(await missing.api.handoff("t3:x")).toBeNull();
    const broken = fakeFetch(() =>
      json({ code: "internal", message: "db locked", next: "retry" }, 500),
    );
    expect((await failure(broken.api.handoff("t3:x"))).code).toBe("internal");
  });
});
