import { describe, expect, test } from "vite-plus/test";
import { DevState } from "../../dev/state.ts";
import { ApiFailure, createApi, type Fetch } from "./client.ts";
import { DISPATCH_HOLD_REASONS, dispatchHoldReason, RUN_KINDS } from "./schema.ts";

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
    expect(detail.notes.length).toBe(3);
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

    const coordinator = agents.find((agent) => agent.handle === "t3:c2d0")?.id ?? "";
    const said = await api.agentSaid(coordinator, 3);
    expect(said.map((item) => item.id)).toEqual([5, 4, 2]);
    expect(said.every((item) => item.agent === coordinator)).toBe(true);
  });

  test("decodes runs, run events, devices, usage buckets, scheduler stats & policy with open enums", async () => {
    const { api } = fakeFetch((url) => {
      const { pathname, searchParams } = new URL(url, "http://board.test");
      if (pathname === "/v1/runs") return json(state.runs(searchParams));
      if (pathname === "/v1/runs/run-01a2b3c4") return json(state.run("run-01a2b3c4"));
      if (pathname === "/v1/runs/run-01a2b3c4/events")
        return json(state.runEvents("run-01a2b3c4", searchParams));
      if (pathname === "/v1/devices") return json(state.devices());
      if (pathname === "/v1/usage/buckets") return json(state.usageBuckets());
      if (pathname === "/v1/scheduler/policy") return json(state.schedulerPolicy());
      if (pathname === "/v1/stats/scheduler") return json(state.schedulerStats(searchParams));
      return json({ code: "not_found", message: url, next: null }, 404);
    });

    const runs = await api.runs({ active: true });
    expect(runs.length).toBe(2);
    expect((await api.run("run-01a2b3c4")).origin).toBe("managed");
    expect((await api.runEvents("run-01a2b3c4")).length).toBe(3);
    expect((await api.devices()).length).toBe(3);
    expect((await api.usageBuckets()).length).toBe(2);
    expect((await api.schedulerPolicy()).version).toBe("v1-default");
    expect((await api.schedulerStats({ days: 7 })).runs_total).toBe(18);

    // Open enums accept unknown values from newer servers without failing decode.
    const futureRun = {
      ...state.run("run-01a2b3c4"),
      kind: "future_kind",
      origin: "future_origin",
      state: "future_state",
    };
    const futureDevice = {
      ...state.devices()[0]!,
      role: "future_role",
      status: "future_status",
      taints: [{ key: "k", value: null, effect: "future_effect" }],
    };
    const futureEvent = {
      ...state.runEvents("run-01a2b3c4", new URLSearchParams())[0]!,
      kind: "future_event_kind",
    };
    const openApi = fakeFetch((url) => {
      if (url.startsWith("/v1/runs/run-01a2b3c4/events")) return json([futureEvent]);
      if (url.startsWith("/v1/runs/")) return json(futureRun);
      if (url === "/v1/devices") return json([futureDevice]);
      return json([], 404);
    }).api;
    expect(await openApi.run("run-01a2b3c4")).toMatchObject({
      kind: "unknown",
      origin: "unknown",
      state: "unknown",
    });
    expect((await openApi.runEvents("run-01a2b3c4"))[0]?.kind).toBe("unknown");
    expect((await openApi.devices())[0]).toMatchObject({
      role: "unknown",
      status: "unknown",
      taints: [{ key: "k", value: null, effect: "unknown" }],
    });
  });

  test("decodes turn endings and rejects bad ones", async () => {
    const said = {
      id: 3,
      agent: "t3:c2d0",
      board: "factory",
      text: "Round done.",
      at: "2026-11-06T12:00:00Z",
    };
    expect(await fakeFetch(() => json([said])).api.agentSaid("t3:c2d0")).toEqual([said]);
    const badSaid = fakeFetch(() => json([{ ...said, id: "3" }]));
    expect((await failure(badSaid.api.agentSaid("t3:c2d0"))).message).toContain(
      '[0].id: expected an integer, got "3"',
    );
  });

  test("builds same-origin URLs, encodes path segments and drops empty params", async () => {
    const { api, calls } = fakeFetch(() => json([]));
    await api.tasks({ board: "web view/x", status: "" });
    await api.tasks({ status: "doing", limit: 5 });
    await api.asks({ status: "open" });
    await api.handoff("t3:7f3a").catch(() => {});
    await api.agentSaid("t3:c2d0", 3).catch(() => {});
    await api.agentSaid("t3:c2d0").catch(() => {});
    expect(calls.map((call) => call.url)).toEqual([
      "/v1/tasks?board=web+view%2Fx&limit=1000",
      "/v1/tasks?status=doing&limit=5",
      "/v1/asks?status=open",
      "/v1/agents/t3%3A7f3a/handoff",
      "/v1/agents/t3%3Ac2d0/said?limit=3",
      "/v1/agents/t3%3Ac2d0/said",
    ]);
    expect(calls[0]?.init?.credentials).toBe("same-origin");
    // No identity or auth headers: the Tailscale header is added by `tailscale serve`.
    expect(Object.keys(calls[0]?.init?.headers ?? {})).toEqual(["accept"]);
  });

  test("sends JSON bodies for writes", async () => {
    const { api, calls } = fakeFetch((_, init) => {
      const raw = typeof init?.body === "string" ? init.body : "";
      const body = JSON.parse(raw) as { autopilot?: boolean; answer?: string };
      if (body.autopilot !== undefined) return json(state.board("factory"));
      return json(state.ask(5));
    });
    await api.setAutopilot("factory", false);
    await api.answerAsk(7, "The existing ops chat");
    expect(calls.map((call) => [call.init?.method, call.url, call.init?.body])).toEqual([
      ["PATCH", "/v1/boards/factory", '{"autopilot":false}'],
      ["POST", "/v1/asks/7/answer", '{"answer":"The existing ops chat"}'],
    ]);
    expect((calls[1]?.init?.headers as Record<string, string> | undefined)?.["content-type"]).toBe(
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

  test("reads ready_auto when sent and null from a server that predates it", async () => {
    const board = state.board("factory");
    const { ready_auto: _, ...older } = board.counts;
    const { api } = fakeFetch(() =>
      json([
        { ...board, counts: { ...board.counts, ready_auto: 2 } },
        { ...board, counts: older },
      ]),
    );
    expect((await api.boards()).map((item) => item.counts.ready_auto)).toEqual([2, null]);
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

  test("decodes v2 additive board and task fields and tolerates older servers omitting them", async () => {
    const baseBoard = state.board("factory");
    const { kind: _bk, curate_every_secs: _bc, ...legacyBoard } = baseBoard;
    const v2Board = { ...legacyBoard, kind: "inbox", curate_every_secs: 43_200 };
    const futureBoard = { ...legacyBoard, kind: "future_kind", curate_every_secs: null };

    const baseTask = state.task("factory-15").task;
    const {
      origin: _to,
      size: _ts,
      cooled_at: _tc,
      ready_at: _tr,
      report_count: _trc,
      effective_priority: _tep,
      ...legacyTask
    } = baseTask;
    const v2Task = {
      ...legacyTask,
      origin: "user",
      size: "xs",
      cooled_at: "2026-11-05T00:00:00Z",
      ready_at: "2026-11-04T00:00:00Z",
      report_count: 7,
      effective_priority: "p1",
    };
    const futureTask = {
      ...legacyTask,
      origin: "future_origin",
      size: "xxl",
      cooled_at: null,
      ready_at: null,
      report_count: 2,
      effective_priority: null,
    };

    const { api } = fakeFetch((url) => {
      if (url === "/v1/boards") return json([legacyBoard, v2Board, futureBoard]);
      if (url.startsWith("/v1/tasks")) return json([legacyTask, v2Task, futureTask]);
      if (url === "/v1/runs/run-curate") {
        return json({ ...state.run("run-01a2b3c4"), id: "run-curate", kind: "curate" });
      }
      return json([], 404);
    });

    const decodedBoards = await api.boards();
    expect(decodedBoards.map((b) => [b.kind, b.curate_every_secs])).toEqual([
      [null, null],
      ["inbox", 43_200],
      ["unknown", null],
    ]);

    const decodedTasks = await api.tasks({ board: "factory" });
    expect(decodedTasks[0]).toMatchObject({
      origin: null,
      size: null,
      cooled_at: null,
      ready_at: null,
      report_count: 0,
      effective_priority: null,
    });
    expect(decodedTasks[1]).toMatchObject({
      origin: "user",
      size: "xs",
      cooled_at: "2026-11-05T00:00:00Z",
      ready_at: "2026-11-04T00:00:00Z",
      report_count: 7,
      effective_priority: "p1",
    });
    expect(decodedTasks[2]).toMatchObject({
      origin: "unknown",
      size: "unknown",
      report_count: 2,
      effective_priority: null,
    });

    expect(RUN_KINDS).toContain("curate");
    expect((await api.run("run-curate")).kind).toBe("curate");
    expect(DISPATCH_HOLD_REASONS).toContain("lease_overlap");
    expect(dispatchHoldReason("lease_overlap", "reason")).toBe("lease_overlap");
    expect(DISPATCH_HOLD_REASONS).toContain("spawn_backoff");
    expect(dispatchHoldReason("spawn_backoff", "reason")).toBe("spawn_backoff");
    expect(dispatchHoldReason("future_hold", "reason")).toBe("unknown");
  });

  test("sends v2 icebox queries, task patches, and board rank mutations", async () => {
    const sample = state.task("factory-16").task;
    const { api, calls } = fakeFetch((url) => {
      if (url.startsWith("/v1/tasks?")) return json([sample]);
      if (url.startsWith("/v1/tasks/")) return json({ ...sample, priority: "p1", version: 99 });
      if (url === "/v1/boards/factory/rank") return json({ tasks: [{ ...sample, rank: 1 }] });
      return json({}, 404);
    });

    await api.listIceboxTasks("factory");
    await api.updateTask("factory-16", {
      version: sample.version,
      priority: "p1",
      cooled: false,
    });
    await api.updateTask("factory-16", {
      version: sample.version,
      status: "dropped",
      reason: "Superseded by v2",
    });
    const ranked = await api.rankBoardTasks("factory", ["factory-16", "factory-15"]);

    expect(ranked.tasks[0]?.rank).toBe(1);
    expect(calls.map((c) => [c.init?.method, c.url, c.init?.body])).toEqual([
      ["GET", "/v1/tasks?board=factory&icebox=true&limit=1000", undefined],
      [
        "PATCH",
        "/v1/tasks/factory-16",
        JSON.stringify({
          agent: "user",
          version: sample.version,
          priority: "p1",
          cooled: false,
        }),
      ],
      [
        "PATCH",
        "/v1/tasks/factory-16",
        JSON.stringify({
          agent: "user",
          version: sample.version,
          status: "dropped",
          reason: "Superseded by v2",
        }),
      ],
      [
        "POST",
        "/v1/boards/factory/rank",
        JSON.stringify({
          agent: "user",
          tasks: ["factory-16", "factory-15"],
          clear_others: false,
        }),
      ],
    ]);
  });

  test("decodes task expedited flag with false default and sends expedited patches", async () => {
    const sample = state.task("factory-16").task;
    const legacy = Object.fromEntries(
      Object.entries(sample).filter(([key]) => key !== "expedited"),
    );
    const { api, calls } = fakeFetch((url, init) => {
      if (url === "/v1/tasks?board=factory&limit=1000") {
        return json([legacy, { ...sample, id: "factory-99", expedited: true }]);
      }
      if (url === "/v1/tasks/factory-16" && init?.method === "PATCH") {
        return json({ ...sample, expedited: true, version: sample.version + 1 });
      }
      return json({}, 404);
    });

    const list = await api.tasks({ board: "factory" });
    expect(list[0]?.expedited).toBe(false);
    expect(list[1]?.expedited).toBe(true);

    const updated = await api.updateTask("factory-16", {
      version: sample.version,
      expedited: true,
    });
    expect(updated.expedited).toBe(true);
    expect(calls[1]).toMatchObject({
      url: "/v1/tasks/factory-16",
      init: {
        method: "PATCH",
        body: JSON.stringify({ agent: "user", version: sample.version, expedited: true }),
      },
    });
  });
});
