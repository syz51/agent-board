// In-memory board state for the dev server: the fixtures plus the derived fields the real
// server computes (counts, `ready`, `owner_stale`, open asks), the few writes the web view
// makes, and a simulator that produces a believable stream of events.

import type {
  Agent,
  Ask,
  Board,
  BoardEvent,
  CancelRunResponse,
  DeviceEntry,
  EventsPage,
  Handoff,
  Run,
  RunEvent,
  Said,
  SchedulerEvaluation,
  SchedulerEvaluationsResponse,
  SchedulerPolicyResponse,
  SchedulerStats,
  SteerRunResponse,
  Task,
  TaskDetail,
  UsageBucket,
} from "../src/api/schema.ts";
import { type BoardFixture, createFixtures, type Fixtures } from "./fixtures.ts";

const STALE_MS = 24 * 3_600_000;
const SAID_LIMIT_MAX = 50;

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

const notFound = (what: string, next: string) =>
  new DevApiError(404, "not_found", `${what} not found`, next);

export class DevState {
  private data: Fixtures;
  private listeners = new Set<(event: BoardEvent) => void>();
  private runListeners = new Map<string, Set<(event: RunEvent) => void>>();
  private step = 0;
  private nextTaskNumber = 32;
  private nextAskId = 10;
  private nextNoteId = 40;
  private nextSaidId = 6;
  private nextWakeId = 100;

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

  onRunEvent(runId: string, listener: (event: RunEvent) => void): () => void {
    let set = this.runListeners.get(runId);
    if (!set) {
      set = new Set();
      this.runListeners.set(runId, set);
    }
    set.add(listener);
    return () => {
      set.delete(listener);
      if (set.size === 0) this.runListeners.delete(runId);
    };
  }

  private pushRunEvent(runId: string, event: Omit<RunEvent, "run_id" | "seq" | "at">): RunEvent {
    const events = (this.data.runEvents[runId] ??= []);
    const nextSeq = (events.at(-1)?.seq ?? 0) + 1;
    const full: RunEvent = { ...event, run_id: runId, seq: nextSeq, at: this.iso() };
    events.push(full);
    const set = this.runListeners.get(runId);
    if (set) {
      for (const listener of set) listener(full);
    }
    return full;
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
    const parent = params.get("parent");
    const icebox = params.get("icebox");
    const limit = Number(params.get("limit") ?? Infinity);
    if (board) this.findBoard(board);
    return this.data.tasks
      .map((task) => this.toTask(task))
      .filter(
        (task) =>
          (!board || task.board === board) &&
          (!status || task.status === status) &&
          (!owner || task.owner === owner) &&
          (!parent || task.parent === parent) &&
          (ready === null || String(task.ready) === ready) &&
          (icebox === "true"
            ? task.status === "open" && task.cooled_at !== null
            : icebox === "false" || icebox === null
              ? task.cooled_at === null
              : true),
      )
      .sort(
        (a, b) =>
          (a.effective_priority ?? a.priority).localeCompare(b.effective_priority ?? b.priority) ||
          Number(a.rank == null) - Number(b.rank == null) ||
          (a.rank ?? 0) - (b.rank ?? 0) ||
          taskNumber(a.id) - taskNumber(b.id),
      )
      .slice(0, limit);
  }

  task(id: string): TaskDetail {
    const stored = this.data.tasks.find((task) => task.id === id);
    if (!stored) throw notFound(`task ${id}`, "check the id; task ids look like factory-12");
    const children = this.data.tasks
      .filter((task) => task.parent === id)
      .map((task) => this.toTask(task))
      .sort((a, b) => taskNumber(a.id) - taskNumber(b.id));
    return {
      task: this.toTask(stored),
      notes: this.data.notes.filter((note) => note.task === id),
      evidence: this.data.evidence
        .filter((item) => item.task === id)
        .map(({ task: _, ...item }) => item),
      blocks: this.data.tasks.filter((task) => task.blockers.includes(id)).map((task) => task.id),
      children: children.map((task) => task.id),
      child_tasks: children.map(({ id, title, status, ready, owner, owner_stale }) => ({
        id,
        title,
        status,
        ready,
        owner,
        owner_stale,
      })),
      history: this.data.events.filter((event) => event.task === id),
    };
  }

  agents(params: URLSearchParams): Agent[] {
    const board = params.get("board");
    const state = params.get("state");
    return this.data.agents.filter(
      (agent) => (!board || agent.boards.includes(board)) && (!state || agent.state === state),
    );
  }

  handoff(agentId: string): Handoff {
    const handoff = this.data.handoffs.find((item) => item.agent === agentId);
    if (!handoff)
      throw notFound(`handoff of ${agentId}`, "the agent has not written a handoff yet");
    return handoff;
  }

  asks(params: URLSearchParams): Ask[] {
    const status = params.get("status");
    const board = params.get("board");
    return this.data.asks.filter(
      (ask) => (!status || ask.status === status) && (!board || ask.board === board),
    );
  }

  ask(id: number): Ask {
    const ask = this.data.asks.find((item) => item.id === id);
    if (!ask) throw notFound(`ask #${id}`, "open /asks for the current ones");
    return ask;
  }

  said(agentId: string, params: URLSearchParams): Said[] {
    if (!this.data.agents.some((agent) => agent.id === agentId)) {
      throw notFound(`agent ${agentId}`, "list agents with GET /v1/agents");
    }
    const limit = Number(params.get("limit") ?? 20);
    if (!Number.isInteger(limit) || limit < 1 || limit > SAID_LIMIT_MAX) {
      throw new DevApiError(
        400,
        "invalid",
        `limit must be 1 to ${SAID_LIMIT_MAX}`,
        "send limit=20",
      );
    }
    return this.data.said
      .filter((item) => item.agent === agentId)
      .sort((a, b) => Date.parse(b.at) - Date.parse(a.at) || b.id - a.id)
      .slice(0, limit);
  }

  events(since: number, limit: number, board: string | null): EventsPage {
    const events = this.eventsSince(since)
      .filter((event) => !board || event.board === board)
      .slice(0, limit);
    return { events, next_since: events.at(-1)?.seq ?? since };
  }

  eventsSince(since: number): BoardEvent[] {
    return this.data.events.filter((event) => event.seq > since);
  }

  runs(params: URLSearchParams): Run[] {
    const board = params.get("board");
    const task = params.get("task");
    const device = params.get("device");
    const kind = params.get("kind");
    const origin = params.get("origin");
    const state = params.get("state");
    const active = params.get("active");
    const limit = Number(params.get("limit") ?? 100);
    const activeSet = new Set(["queued", "leased", "starting", "running", "waiting_external"]);
    return this.data.runs
      .filter(
        (run) =>
          (!board || run.board === board) &&
          (!task || run.task_id === task) &&
          (!device || run.device === device) &&
          (!kind || run.kind === kind) &&
          (!origin || run.origin === origin) &&
          (!state || run.state === state) &&
          (active === null || activeSet.has(run.state) === (active === "true")),
      )
      .slice(0, limit);
  }

  run(id: string): Run {
    const found = this.data.runs.find((item) => item.id === id);
    if (!found) throw notFound(`run ${id}`, "list runs with GET /v1/runs");
    return found;
  }

  runEvents(id: string, params: URLSearchParams): RunEvent[] {
    this.run(id);
    const afterSeq = Number(params.get("after_seq") ?? 0);
    const limit = Number(params.get("limit") ?? 200);
    return (this.data.runEvents[id] ?? []).filter((ev) => ev.seq > afterSeq).slice(0, limit);
  }

  steerRun(id: string, body: unknown): SteerRunResponse {
    const target = this.run(id);
    const message = isRecord(body) && typeof body.message === "string" ? body.message.trim() : "";
    if (!message) {
      throw new DevApiError(400, "invalid", "message is empty", "pass a non-empty message");
    }
    const wake_id = this.nextWakeId++;
    this.pushRunEvent(id, {
      kind: "message",
      summary: `Steer queued: ${message.slice(0, 200)}`,
      meta: { wake_id },
    });
    this.emit({
      board: target.board,
      actor: "user",
      type: "run_steered",
      task: target.task_id,
      payload: { run_id: id, wake_id },
    });
    return { wake_id, run_id: id, device: target.device };
  }

  cancelRun(id: string, body: unknown): CancelRunResponse {
    const target = this.run(id);
    const reason = isRecord(body) && typeof body.reason === "string" ? body.reason.trim() : null;
    if (target.state === "queued") {
      target.state = "cancelled";
      target.ended_at = this.iso();
      this.pushRunEvent(id, {
        kind: "cancelled",
        summary: reason ? `Cancelled: ${reason}` : "Cancelled before start",
        meta: null,
      });
      this.emit({
        board: target.board,
        actor: "user",
        type: "run_cancelled",
        task: target.task_id,
        payload: { run_id: id, reason },
      });
      return { run_id: id, device: target.device, state: "cancelled", wake_id: null };
    }
    const wake_id = this.nextWakeId++;
    this.pushRunEvent(id, {
      kind: "note",
      summary: reason ? `Cancel requested: ${reason}` : "Cancel requested",
      meta: { wake_id },
    });
    this.emit({
      board: target.board,
      actor: "user",
      type: "run_cancelled",
      task: target.task_id,
      payload: { run_id: id, wake_id, reason },
    });
    return { run_id: id, device: target.device, state: target.state, wake_id };
  }

  devices(): DeviceEntry[] {
    return this.data.devices;
  }

  usageBuckets(): UsageBucket[] {
    return this.data.usageBuckets;
  }

  schedulerPolicy(): SchedulerPolicyResponse {
    return this.data.schedulerPolicy;
  }

  schedulerStats(params: URLSearchParams): SchedulerStats {
    const days = Number(params.get("days") ?? 7);
    return { ...this.data.schedulerStats, days: Number.isFinite(days) ? days : 7 };
  }

  schedulerEvaluations(params: URLSearchParams): SchedulerEvaluationsResponse {
    const rawLimit = Number(params.get("limit") ?? 20);
    const limit = Number.isInteger(rawLimit) && rawLimit >= 1 ? Math.min(rawLimit, 200) : 20;
    return {
      ...this.data.schedulerEvaluations,
      evaluations: this.data.schedulerEvaluations.evaluations.slice(0, limit),
    };
  }

  runSchedulerEvaluation(): SchedulerEvaluation {
    const nextId =
      this.data.schedulerEvaluations.evaluations.reduce((m, e) => Math.max(m, e.id), 0) + 1;
    const evaluation: SchedulerEvaluation = {
      id: nextId,
      evaluated_at: this.iso(),
      mode: this.data.schedulerEvaluations.mode,
      decision: "keep",
      baseline_policy_version: this.data.schedulerPolicy.version,
      candidate_policy_version: "fair-boost",
      applied_policy_version: null,
      previous_policy_version: null,
      window_days: 7,
      sample_runs: this.data.schedulerStats.runs_total,
      sample_tasks: this.data.tasks.length,
      baseline_metrics: null,
      candidate_metrics: null,
      candidate_weights: null,
      score_delta_pct: 1.2,
      reason: "Manual evaluation: best candidate fair-boost improved +1.2% < threshold 5.0%",
      feedback_task_id: null,
    };
    this.data.schedulerEvaluations.evaluations.unshift(evaluation);
    this.data.schedulerStats.latest_evaluation = evaluation;
    return evaluation;
  }

  // Writes the web view makes.

  setAutopilot(slug: string, body: unknown): Board {
    const board = this.findBoard(slug);
    const autopilot = isRecord(body) ? body.autopilot : undefined;
    if (typeof autopilot !== "boolean") {
      throw new DevApiError(
        400,
        "invalid",
        "autopilot must be true or false",
        'send {"autopilot": true}',
      );
    }
    if (board.autopilot !== autopilot) {
      board.autopilot = autopilot;
      board.updated_at = this.iso();
      this.emit({
        board: slug,
        actor: "user",
        type: "board_updated",
        task: null,
        payload: { autopilot },
      });
    }
    return this.toBoard(board);
  }

  answerAsk(id: number, body: unknown): Ask {
    const ask = this.ask(id);
    const answer = isRecord(body) && typeof body.answer === "string" ? body.answer.trim() : "";
    if (!answer)
      throw new DevApiError(400, "invalid", "answer is empty", "write an answer or pick an option");
    if (ask.status !== "open") {
      throw new DevApiError(
        400,
        "invalid",
        `ask #${id} is already ${ask.status}`,
        "reload to see the answer",
      );
    }
    Object.assign(ask, {
      status: "answered",
      answer,
      answered_via: "web",
      answered_at: this.iso(),
    });
    this.emit({
      board: ask.board,
      actor: "user",
      type: "ask_answered",
      task: ask.task,
      payload: { ask: id, answered_via: "web" },
    });
    return ask;
  }

  updateTask(id: string, body: unknown): Task {
    const task = this.findTask(id);
    if (!isRecord(body)) {
      throw new DevApiError(400, "invalid", "expected a JSON object", "send a task patch");
    }
    if (typeof body.version === "number" && body.version !== task.version) {
      throw new DevApiError(
        409,
        "version_conflict",
        `${id} changed (version ${task.version}, sent ${body.version})`,
        "reload the task and try again",
      );
    }
    const nowIso = this.iso();
    const fields: string[] = [];
    if (body.priority === "p1" || body.priority === "p2" || body.priority === "p3") {
      if (task.priority !== body.priority) {
        task.priority = body.priority;
        task.rank = null;
        fields.push("priority");
      }
    }
    if (
      body.size === null ||
      body.size === "xs" ||
      body.size === "s" ||
      body.size === "m" ||
      body.size === "l"
    ) {
      task.size = body.size;
      fields.push("size");
    }
    if (
      body.rank === null ||
      (typeof body.rank === "number" && Number.isInteger(body.rank) && body.rank >= 1)
    ) {
      task.rank = body.rank;
      fields.push("rank");
    }
    if (typeof body.cooled === "boolean") {
      task.cooled_at = body.cooled ? nowIso : null;
      fields.push("cooled_at");
    }
    if (typeof body.auto === "boolean") {
      task.auto = body.auto;
      fields.push("auto");
    }
    if (
      body.status === "open" ||
      body.status === "doing" ||
      body.status === "blocked" ||
      body.status === "review" ||
      body.status === "done" ||
      body.status === "dropped"
    ) {
      if (body.status === "dropped") {
        const reason = typeof body.reason === "string" ? body.reason.trim() : "";
        if (!reason) {
          throw new DevApiError(
            400,
            "invalid",
            "reason is required to drop a task",
            "provide a short reason",
          );
        }
        this.data.notes.push({
          id: this.nextNoteId++,
          task: task.id,
          agent: "user",
          kind: "decision",
          text: `Dropped: ${reason}`,
          at: nowIso,
        });
      }
      task.status = body.status;
      fields.push("status");
    }
    task.version += 1;
    task.updated_at = nowIso;
    this.emit({
      board: task.board,
      actor: "user",
      type: "task_updated",
      task: task.id,
      payload: { fields },
    });
    return this.toTask(task);
  }

  rankTasks(slug: string, body: unknown): { tasks: Task[] } {
    this.findBoard(slug);
    if (!isRecord(body) || !Array.isArray(body.tasks)) {
      throw new DevApiError(400, "invalid", "tasks must be an array of task ids", "pass task ids");
    }
    const ids = body.tasks.filter((item): item is string => typeof item === "string");
    const clearOthers = body.clear_others === true;
    if (clearOthers) {
      const keep = new Set(ids);
      for (const task of this.data.tasks) {
        if (task.board === slug && !keep.has(task.id)) task.rank = null;
      }
    }
    const perPriority = new Map<string, number>();
    const updated: Task[] = [];
    const nowIso = this.iso();
    for (const id of ids) {
      const task = this.findTask(id);
      if (task.board !== slug) {
        throw new DevApiError(
          400,
          "invalid",
          `${id} is not on board ${slug}`,
          "rank tasks on one board",
        );
      }
      const nextRank = (perPriority.get(task.priority) ?? 0) + 1;
      perPriority.set(task.priority, nextRank);
      task.rank = nextRank;
      task.version += 1;
      task.updated_at = nowIso;
      updated.push(this.toTask(task));
    }
    this.emit({
      board: slug,
      actor: "user",
      type: "task_updated",
      task: ids[0] ?? null,
      payload: { fields: ["rank"], ranked: ids.length },
    });
    return { tasks: updated };
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
      () => this.coordinatorSays(),
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
    return this.emit({
      board: null,
      actor: agent.id,
      type: "agent_state_changed",
      task: null,
      payload: { from, to: agent.state },
    });
  }

  private claimReady(): BoardEvent | null {
    const ready = this.tasks(new URLSearchParams({ board: "factory", ready: "true" }))[0];
    const worker = this.data.agents.find((item) => item.handle === "t3:a91c");
    if (!ready || !worker) return null;
    const task = this.findTask(ready.id);
    Object.assign(task, {
      status: "doing",
      owner: worker.id,
      updated_at: this.iso(),
      version: task.version + 1,
    });
    worker.state = "running";
    return this.emit({
      board: task.board,
      actor: worker.id,
      type: "task_claimed",
      task: task.id,
      payload: { from: "open", to: "doing" },
    });
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
    const task = this.data.tasks.find(
      (item) =>
        item.status === "doing" &&
        item.board === "factory" &&
        item.id !== "factory-12" &&
        item.id !== "factory-14",
    );
    if (!task?.owner) return null;
    Object.assign(task, { status: "review", updated_at: this.iso(), version: task.version + 1 });
    return this.emit({
      board: task.board,
      actor: task.owner,
      type: "task_updated",
      task: task.id,
      payload: { from: "doing", to: "review" },
    });
  }

  private addNote(): BoardEvent | null {
    const task = this.data.tasks.find(
      (item) => item.status === "review" && item.board === "factory",
    );
    if (!task?.owner) return null;
    this.data.notes.push({
      id: this.nextNoteId++,
      task: task.id,
      agent: task.owner,
      kind: "finding",
      text: "Checks pass locally; waiting for review.",
      at: this.iso(),
    });
    return this.emit({
      board: task.board,
      actor: task.owner,
      type: "note_added",
      task: task.id,
      payload: { kind: "finding" },
    });
  }

  private finishReview(): BoardEvent | null {
    const task = this.data.tasks.find(
      (item) => item.status === "review" && item.board === "factory",
    );
    if (!task?.owner) return null;
    const at = this.iso();
    this.data.evidence.push({
      task: task.id,
      kind: "check",
      value: "cargo test --workspace",
      exit_code: 0,
      agent: task.owner,
      at,
    });
    this.emit({
      board: task.board,
      actor: task.owner,
      type: "evidence_added",
      task: task.id,
      payload: { kind: "check", exit_code: 0 },
    });
    Object.assign(task, {
      status: "done",
      updated_at: at,
      version: task.version + 1,
      summary: "Review passed and `cargo test --workspace` is green. Nothing needs you.",
    });
    return this.emit({
      board: task.board,
      actor: task.owner,
      type: "task_done",
      task: task.id,
      payload: { from: "review", to: "done" },
    });
  }

  private createTask(): BoardEvent | null {
    const open = this.data.tasks.filter(
      (task) => task.board === "factory" && task.status === "open",
    ).length;
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
      rank: null,
      effective_priority: null,
      origin: "agent",
      size: null,
      cooled_at: null,
      ready_at: at,
      report_count: 0,
      blocked_reason: null,
      owner: null,
      blockers: [],
      not_before: null,
      auto: true,
      created_by: coordinator.id,
      version: 1,
      created_at: at,
      updated_at: at,
      feedback: null,
      summary: null,
    });
    return this.emit({
      board: "factory",
      actor: coordinator.id,
      type: "task_created",
      task: id,
      payload: {},
    });
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
    return this.emit({
      board: "factory",
      actor: coordinator.id,
      type: "ask_created",
      task: null,
      payload: { ask: id, kind: "decision" },
    });
  }

  /** A coordinator ends its turn: what it said is kept, and its state change is the event. */
  private coordinatorSays(): BoardEvent | null {
    const coordinator = this.data.agents.find((item) => item.handle === "t3:c2d0");
    if (!coordinator) return null;
    const from = coordinator.state;
    if (from === "running") {
      this.data.said.push({
        id: this.nextSaidId++,
        agent: coordinator.id,
        board: "factory",
        text: `Round ${this.nextSaidId - 5} done. Ready tasks are assigned; **nothing needs you** right now.`,
        at: this.iso(),
      });
    }
    coordinator.state = from === "running" ? "idle" : "running";
    coordinator.last_seen = this.iso();
    return this.emit({
      board: null,
      actor: coordinator.id,
      type: "agent_state_changed",
      task: null,
      payload: { from, to: coordinator.state },
    });
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
    const status = (id: string) => this.data.tasks.find((item) => item.id === id)?.status;
    const done = (id: string) => status(id) === "done";
    const dropped = (id: string) => status(id) === "dropped";
    const owner = this.data.agents.find((item) => item.id === task.owner);
    // The dev server stands in for the Rust one, so it mirrors `is_pending_proposal` in
    // crates/board-proto/src/model.rs; the web view itself only reads `proposal`.
    const proposal =
      task.status === "open" &&
      !task.auto &&
      task.feedback === null &&
      task.body.trimStart().startsWith("Proposal:");
    return {
      ...task,
      owner_stale: owner !== undefined && this.stale(owner),
      owner_handle: owner?.handle ?? null,
      proposal,
      child_count: this.data.tasks.filter((item) => item.parent === task.id).length,
      snippet: null,
      any_word: false,
      // Mirrors `complete_tasks` in crates/board-server/src/store.rs.
      blockers_closed:
        task.blockers.length > 0 &&
        task.blockers.every((id) => done(id) || dropped(id)) &&
        (task.status === "blocked" || (task.status === "open" && task.blockers.some(dropped))),
      ready:
        task.status === "open" &&
        task.cooled_at === null &&
        !proposal &&
        task.blockers.every(done) &&
        !this.ownerLive(task.owner) &&
        (task.not_before === null || Date.parse(task.not_before) <= this.now()),
    };
  }

  private toBoard(board: BoardFixture): Board {
    const tasks = this.data.tasks
      .filter((task) => task.board === board.slug && task.cooled_at === null)
      .map((task) => this.toTask(task));
    const count = (status: Task["status"]) => tasks.filter((task) => task.status === status).length;
    return {
      ...board,
      kind: "project",
      goal_review_every_secs: null,
      curate_every_secs: 86_400,
      counts: {
        open: count("open"),
        doing: count("doing"),
        blocked: count("blocked"),
        review: count("review"),
        done: count("done"),
        dropped: count("dropped"),
        ready: tasks.filter((task) => task.ready).length,
        ready_auto: tasks.filter((task) => task.ready && task.auto).length,
      },
      open_asks: this.data.asks.filter((ask) => ask.board === board.slug && ask.status === "open")
        .length,
    };
  }
}

function taskNumber(id: string): number {
  return Number(id.slice(id.lastIndexOf("-") + 1));
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
