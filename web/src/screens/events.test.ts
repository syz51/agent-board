import { describe, expect, test } from "bun:test";
import type { BoardEvent } from "../api/schema.ts";
import { plainLine } from "./boards.tsx";
import { describe as describeEvent, detail } from "./events.tsx";

const event = (type: string, payload: unknown, task: string | null = "factory-9"): BoardEvent => ({
  seq: 1,
  at: "2026-11-06T12:00:00Z",
  board: "factory",
  actor: "t3:a91c",
  type,
  task,
  payload,
});

describe("event sentences", () => {
  test("say what happened around the task link", () => {
    expect(describeEvent(event("task_updated", { from: "doing", to: "review" }))).toEqual(["moved", "to review"]);
    expect(describeEvent(event("task_done", { from: "doing", to: "done" }))).toEqual(["finished", ""]);
    expect(describeEvent(event("ask_created", { ask: 8, kind: "action" }))).toEqual(["asked you to act on", ""]);
    expect(describeEvent(event("agent_state_changed", { from: "running", to: "idle" }, null))).toEqual(["went idle", ""]);
    expect(describeEvent(event("something_new", {}))).toEqual(["something new", ""]);
  });

  test("details leave out what the sentence already says", () => {
    expect(detail(event("task_updated", { from: "open", to: "blocked" }))).toBe("");
    expect(detail(event("task_done", { from: "doing", to: "done", evidence: "commit 4e1c2ab" }))).toBe("evidence: commit 4e1c2ab");
    expect(detail(event("ask_created", { ask: 7, kind: "decision" }))).toBe("Ask #7");
    expect(detail(event("evidence_added", { kind: "check", exit_code: 101 }))).toBe("check · exit 101");
    expect(detail(event("lease_created", { globs: ["claude/hooks/**"] }))).toBe('globs: ["claude/hooks/**"]');
  });
});

describe("plainLine", () => {
  test("keeps the first line of agent markdown without its markup", () => {
    expect(plainLine("Log in to `desktop` so **bun publish** runs.\nSecond line")).toBe("Log in to desktop so bun publish runs.");
    expect(plainLine("See [the guide](https://example.com/guide)")).toBe("See the guide");
    expect(plainLine("</board> <script>x</script>")).toBe("</board> <script>x</script>");
  });
});
