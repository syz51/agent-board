// In-memory board state for the dev server: the fixtures plus the derived fields the real
// server computes (counts, `ready`, `owner_stale`, open asks), the few writes the web view
// makes, and a simulator that produces a believable stream of events.

import type {
  Agent,
  Ask,
  Board,
  BoardEvent,
  EventsPage,
  Handoff,
  Task,
  TaskDetail,
} from "../src/api/schema.ts";
import { type BoardFixture, createFixtures, type Fixtures } from "./fixtures.ts";

const STALE_MS = 24 * 3_600_000;

/** An error the dev server returns as an `ApiError` body. */
export class DevApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly next: string | null,
  ) {
    super(message);
  }

  body() {
    return { code: this.code, message: this.message, next: this.next };
  }
}

const notFound = (what: string, next: string) => new DevApiError(404, "not_found", `${what} not found`, next);

export class DevState {
  private data: Fixtures;
  private listeners = new Set<(event: BoardEvent) => void>();
  private step = 0;
  private nextTaskNumber = 30;
  private nextAskId = 10;
  private nextNoteId = 40;

  constructor(private readonly now: () => number = Date.now) {
    this.data = createFixtures(now());
  }

  get seq(): number {
    return this.data.events.at(-1)?.seq ?? 0;
  }

  onEvent(listener: (event: BoardEvent) => void): () => void {
    this.listeners.add(listener);
    return () => void this.listeners.delete(listener);
  }

  // Reads, in the shapes of the HTTP API.

  boards(): Board[] {
    return this.data.boards.map((board) => this.toBoard(board));
  }

  board(slug: string): Board {
    return this.toBoard(this.findBoard(slug));
  }

  tasks(params: URLSearchParams): Task[] {
    const board = params.get("board");
    const status = params.get("status");
    const owner = params.get("owner");
    const ready = params.get("ready");
    const limit = Number(params.get("limit") ?? Infinity);
    if (board) this.findBoard(board);
    return this.data.tasks
      .map((task) => this.toTask(task))
      .filter((task) =>
        (!board || task.board === board) && (!status || task.status === status) &&
        (!owner || task.owner === owner) && (ready === null || String(task.ready) === ready)
      )
      .sort((a, b) => a.priority.localeCompare(b.priority) || taskNumber(a.id) - taskNumber(b.id))
      .slice(0, limit);
  }

  task(id: string): TaskDetail {
    const stored = this.data.tasks.find((task) => task.id === id);
    if (!stored) throw notFound(`task ${id}`, "check the id; task ids look like factory-12");
    return {
      task: this.toTask(stored),
      notes: this.data.notes.filter((note) => note.task === id),
      evidence: this.data.evidence.filter((item) => item.task === id).map(({ task: _, ...item }) => item),
      blocks: this.data.tasks.filter((task) => task.blockers.includes(id)).map((task) => task.id),
      children: this.data.tasks.filter((task) => task.parent === id).map((task) => task.id),
      history: this.data.events.filter((event) => event.task === id),
    };
  }

  agents(params: URLSearchParams): Agent[] {
    const board = params.get("board");
    const state = params.get("state");
    return this.data.agents.filter((agent) =>
      (!board || agent.boards.includes(board)) && (!state || agent.state === state)
    );
  }

  handoff(agentId: string): Handoff {
    const handoff = this.data.handoffs.find((item) => item.agent === agentId);
    if (!handoff) throw notFound(`handoff of ${agentId}`, "the agent has not written a handoff yet");
    return handoff;
  }

  asks(params: URLSearchParams): Ask[] {
    const status = params.get("status");
    const board = params.get("board");
    return this.data.asks.filter((ask) => (!status || ask.status === status) && (!board || ask.board === board));
  }

  ask(id: number): Ask {
    const ask = this.data.asks.find((item) => item.id === id);
    if (!ask) throw notFound(`ask #${id}`, "open /asks for the current ones");
    return ask;
  }

  events(since: number, limit: number, board: string | null): EventsPage {
    const events = this.eventsSince(since).filter((event) => !board || event.board === board).slice(0, limit);
    return { events, next_since: events.at(-1)?.seq ?? since };
  }

  eventsSince(since: number): BoardEvent[] {
    return this.data.events.filter((event) => event.seq > since);
  }

  // Writes the web view makes.

  setAutopilot(slug: string, body: unknown): Board {
    const board = this.findBoard(slug);
    const autopilot = isRecord(body) ? body.autopilot : undefined;
    if (typeof autopilot !== "boolean") {
      throw new DevApiError(400, "invalid", "autopilot must be true or false", "send {\"autopilot\": true}");
    }
    if (board.autopilot !== autopilot) {
      board.autopilot = autopilot;
      board.updated_at = this.iso();
      this.emit({ board: slug, actor: "user", type: "board_updated", task: null, payload: { autopilot } });
    }
    return this.toBoard(board);
  }

  answerAsk(id: number, body: unknown): Ask {
    const ask = this.ask(id);
    const answer = isRecord(body) && typeof body.answer === "string" ? body.answer.trim() : "";
    if (!answer) throw new DevApiError(400, "invalid", "answer is empty", "write an answer or pick an option");
    if (ask.status !== "open") {
      throw new DevApiError(400, "invalid", `ask #${id} is already ${ask.status}`, "reload to see the answer");
    }
    Object.assign(ask, { status: "answered", answer, answered_via: "web", answered_at: this.iso() });
    this.emit({
      board: ask.board,
      actor: "user",
      type: "ask_answered",
      task: ask.task,
      payload: { ask: id, answered_via: "web" },
    });
    return ask;
  }

  // The simulator: one plausible change per call.

  simulate(): BoardEvent | null {
    const actions = [
      () => this.heartbeat(),
      () => this.claimReady(),
      () => this.touchFiles(),
      () => this.moveToReview(),
      () => this.addNote(),
      () => this.finishReview(),
      () => this.createTask(),
      () => this.createAsk(),
    ];
    for (let tried = 0; tried < actions.length; tried++) {
      const action = actions[this.step++ % actions.length];
      const event = action?.();
      if (event) return event;
    }
    return null;
  }

  private heartbeat(): BoardEvent | null {
    const agent = this.data.agents.find((item) => item.handle === "t3:a91c");
    if (!agent) return null;
    const from = agent.state;
    agent.state = from === "idle" ? "running" : "idle";
    agent.last_seen = this.iso();
    return this.emit({ board: null, actor: agent.id, type: "agent_state_changed", task: null, payload: { from, to: agent.state } });
  }

  private claimReady(): BoardEvent | null {
    const ready = this.tasks(new URLSearchParams({ board: "factory", ready: "true" }))[0];
    const worker = this.data.agents.find((item) => item.handle === "t3:a91c");
    if (!ready || !worker) return null;
    const task = this.findTask(ready.id);
    Object.assign(task, { status: "doing", owner: worker.id, updated_at: this.iso(), version: task.version + 1 });
    worker.state = "running";
    return this.emit({ board: task.board, actor: worker.id, type: "task_claimed", task: task.id, payload: { from: "open", to: "doing" } });
  }

  private touchFiles(): BoardEvent | null {
    const agent = this.data.agents[0];
    if (!agent) return null;
    return this.emit({
      board: "factory",
      actor: agent.id,
      type: "files_touched",
      task: "factory-12",
      payload: { paths: ["claude/hooks/delta.js"] },
    });
  }

  private moveToReview(): BoardEvent | null {
    const task = this.data.tasks.find((item) => item.status === "doing" && item.board === "factory" && item.id !== "factory-12" && item.id !== "factory-14");
    if (!task?.owner) return null;
    Object.assign(task, { status: "review", updated_at: this.iso(), version: task.version + 1 });
    return this.emit({ board: task.board, actor: task.owner, type: "task_updated", task: task.id, payload: { from: "doing", to: "review" } });
  }

  private addNote(): BoardEvent | null {
    const task = this.data.tasks.find((item) => item.status === "review" && item.board === "factory");
    if (!task?.owner) return null;
    this.data.notes.push({
      id: this.nextNoteId++,
      task: task.id,
      agent: task.owner,
      kind: "finding",
      text: "Checks pass locally; waiting for review.",
      at: this.iso(),
    });
    return this.emit({ board: task.board, actor: task.owner, type: "note_added", task: task.id, payload: { kind: "finding" } });
  }

  private finishReview(): BoardEvent | null {
    const task = this.data.tasks.find((item) => item.status === "review" && item.board === "factory");
    if (!task?.owner) return null;
    const at = this.iso();
    this.data.evidence.push({ task: task.id, kind: "check", value: "cargo test --workspace", exit_code: 0, agent: task.owner, at });
    this.emit({ board: task.board, actor: task.owner, type: "evidence_added", task: task.id, payload: { kind: "check", exit_code: 0 } });
    Object.assign(task, { status: "done", updated_at: at, version: task.version + 1 });
    return this.emit({ board: task.board, actor: task.owner, type: "task_done", task: task.id, payload: { from: "review", to: "done" } });
  }

  private createTask(): BoardEvent | null {
    const open = this.data.tasks.filter((task) => task.board === "factory" && task.status === "open").length;
    const coordinator = this.data.agents.find((item) => item.handle === "t3:c2d0");
    if (open >= 6 || !coordinator) return null;
    const id = `factory-${this.nextTaskNumber++}`;
    const at = this.iso();
    this.data.tasks.push({
      id,
      board: "factory",
      parent: null,
      title: "Follow up on a finding from review",
      body: "",
      acceptance: "",
      status: "open",
      priority: "p2",
      blocked_reason: null,
      owner: null,
      blockers: [],
      auto: true,
      created_by: coordinator.id,
      version: 1,
      created_at: at,
      updated_at: at,
    });
    return this.emit({ board: "factory", actor: coordinator.id, type: "task_created", task: id, payload: {} });
  }

  private createAsk(): BoardEvent | null {
    if (this.data.asks.filter((ask) => ask.status === "open").length >= 5) return null;
    const coordinator = this.data.agents.find((item) => item.handle === "t3:c2d0");
    if (!coordinator) return null;
    const id = this.nextAskId++;
    this.data.asks.push({
      id,
      board: "factory",
      task: null,
      agent: coordinator.id,
      kind: "decision",
      question: "Two ready tasks touch the same files. Which goes first?",
      options: ["Leases first", "Import plan first"],
      status: "open",
      answer: null,
      answered_via: null,
      created_at: this.iso(),
      answered_at: null,
    });
    return this.emit({ board: "factory", actor: coordinator.id, type: "ask_created", task: null, payload: { ask: id, kind: "decision" } });
  }

  // Helpers.

  private emit(event: Omit<BoardEvent, "seq" | "at">): BoardEvent {
    const full: BoardEvent = { ...event, seq: this.seq + 1, at: this.iso() };
    this.data.events.push(full);
    for (const listener of this.listeners) listener(full);
    return full;
  }

  private iso(): string {
    return new Date(this.now()).toISOString();
  }

  private findBoard(slug: string): BoardFixture {
    const board = this.data.boards.find((item) => item.slug === slug);
    if (!board) throw notFound(`board ${slug}`, "list boards with GET /v1/boards");
    return board;
  }

  private findTask(id: string) {
    const task = this.data.tasks.find((item) => item.id === id);
    if (!task) throw notFound(`task ${id}`, "check the id");
    return task;
  }

  private ownerLive(owner: string | null): boolean {
    const agent = this.data.agents.find((item) => item.id === owner);
    return agent !== undefined && agent.state !== "ended" && !this.stale(agent);
  }

  private stale(agent: Agent): boolean {
    return this.now() - Date.parse(agent.last_seen) > STALE_MS;
  }

  private toTask(task: Fixtures["tasks"][number]): Task {
    const done = (id: string) => this.data.tasks.find((item) => item.id === id)?.status === "done";
    const owner = this.data.agents.find((item) => item.id === task.owner);
    return {
      ...task,
      owner_stale: owner !== undefined && this.stale(owner),
      ready: task.status === "open" && task.blockers.every(done) && !this.ownerLive(task.owner),
    };
  }

  private toBoard(board: BoardFixture): Board {
    const tasks = this.data.tasks.filter((task) => task.board === board.slug).map((task) => this.toTask(task));
    const count = (status: Task["status"]) => tasks.filter((task) => task.status === status).length;
    return {
      ...board,
      counts: {
        open: count("open"),
        doing: count("doing"),
        blocked: count("blocked"),
        review: count("review"),
        done: count("done"),
        dropped: count("dropped"),
        ready: tasks.filter((task) => task.ready).length,
      },
      open_asks: this.data.asks.filter((ask) => ask.board === board.slug && ask.status === "open").length,
    };
  }
}

function taskNumber(id: string): number {
  return Number(id.slice(id.lastIndexOf("-") + 1));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
