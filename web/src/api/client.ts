// The only place that talks HTTP. Calls are same-origin; the browser's identity comes from
// the `Tailscale-User-Login` header that `tailscale serve` adds, so nothing extra is sent.

import { array, type Decoder, DecodeError } from "./decode.ts";
import * as s from "./schema.ts";

/** A failed call: the server's `ApiError`, or a client-side failure with the same shape. */
export class ApiFailure extends Error {
  constructor(
    /** HTTP status, or 0 when the server could not be reached. */
    readonly status: number,
    /** `ErrorCode` from the server, or `network` / `bad_response` / `http_<status>`. */
    readonly code: string,
    message: string,
    /** What to do next, when known. */
    readonly next: string | null,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "ApiFailure";
  }
}

export type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

const seg = encodeURIComponent;

/** Largest `limit` `GET /v1/tasks` accepts. */
export const MAX_TASKS = 1000;

function query(params: Record<string, string | number | boolean | undefined>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== "") search.set(key, String(value));
  }
  const text = search.toString();
  return text ? `?${text}` : "";
}

export function createApi(fetchImpl: Fetch = (input, init) => fetch(input, init)) {
  async function request<T>(
    method: string,
    path: string,
    decode: Decoder<T>,
    body?: unknown,
  ): Promise<T> {
    let response: Response;
    try {
      response = await fetchImpl(path, {
        method,
        credentials: "same-origin",
        headers:
          body === undefined
            ? { accept: "application/json" }
            : {
                accept: "application/json",
                "content-type": "application/json",
              },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
    } catch (cause) {
      throw new ApiFailure(
        0,
        "network",
        "Could not reach the board server.",
        "Check that this device is on the tailnet; the page retries when the connection returns.",
        { cause },
      );
    }

    const text = await response.text();
    if (!response.ok) throw failureFrom(response, text);

    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch (cause) {
      throw badResponse(response.status, path, "the body is not JSON", cause);
    }
    try {
      return decode(parsed, "");
    } catch (cause) {
      if (cause instanceof DecodeError)
        throw badResponse(response.status, path, cause.message, cause);
      throw cause;
    }
  }

  const get = <T>(path: string, decode: Decoder<T>) => request("GET", path, decode);

  return {
    health: () => get("/healthz", s.health),
    boards: () => get("/v1/boards", array(s.board)),
    board: (slug: string) => get(`/v1/boards/${seg(slug)}`, s.board),
    setAutopilot: (slug: string, autopilot: boolean) =>
      request("PATCH", `/v1/boards/${seg(slug)}`, s.board, { autopilot }),
    // The server defaults to 200 tasks; screens ask for its maximum so a busy board is complete.
    tasks: (params: {
      board?: string;
      status?: string;
      owner?: string;
      parent?: string;
      limit?: number;
    }) => get(`/v1/tasks${query({ ...params, limit: params.limit ?? MAX_TASKS })}`, array(s.task)),
    task: (id: string) => get(`/v1/tasks/${seg(id)}`, s.taskDetail),
    agents: (params: { board?: string; state?: string } = {}) =>
      get(`/v1/agents${query(params)}`, array(s.agent)),
    /** `null` when the agent has not written a handoff yet (404). */
    handoff: async (agentId: string) => {
      try {
        return await get(`/v1/agents/${seg(agentId)}/handoff`, s.handoff);
      } catch (error) {
        if (error instanceof ApiFailure && error.status === 404) return null;
        throw error;
      }
    },
    asks: (params: { status?: string; board?: string }) =>
      get(`/v1/asks${query(params)}`, array(s.ask)),
    ask: (id: number) => get(`/v1/asks/${id}`, s.ask),
    answerAsk: (id: number, answer: string) =>
      request("POST", `/v1/asks/${id}/answer`, s.ask, { answer }),
    /** An agent's newest turn endings, newest first (`limit` 1 to 50). */
    agentSaid: (agentId: string, limit?: number) =>
      get(`/v1/agents/${seg(agentId)}/said${query({ limit })}`, array(s.said)),
    events: (params: { since: number; limit?: number; board?: string }) =>
      get(`/v1/events${query(params)}`, s.eventsPage),
  };
}

export type Api = ReturnType<typeof createApi>;

function failureFrom(response: Response, text: string): ApiFailure {
  try {
    const body = s.apiError(JSON.parse(text), "");
    return new ApiFailure(response.status, body.code, body.message, body.next);
  } catch {
    // Not an ApiError body, for example a proxy page: describe the HTTP status instead.
    const status = `${response.status} ${response.statusText}`.trim();
    return new ApiFailure(
      response.status,
      `http_${response.status}`,
      `The server answered ${status}.`,
      response.status >= 500 ? "The server may be restarting; try again shortly." : null,
    );
  }
}

function badResponse(status: number, path: string, detail: string, cause: unknown): ApiFailure {
  return new ApiFailure(
    status,
    "bad_response",
    `Unexpected response from ${path}: ${detail}.`,
    "The web view may be older than the server; reload the page.",
    { cause },
  );
}

export const api = createApi();
