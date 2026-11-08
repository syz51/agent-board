import { afterEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderInApp } from "../../test/render.tsx";
import { queries } from "../live/query.ts";
import { HomeScreen, inProgress } from "./boards.tsx";

const now = Date.parse("2026-11-06T12:00:00Z");
const state = new DevState(() => now);
const none = new URLSearchParams();
const active = () => [
  ...state.tasks(new URLSearchParams({ status: "doing" })),
  ...state.tasks(new URLSearchParams({ status: "review" })),
];

afterEach(() => localStorage.clear());

function seed(client: QueryClient) {
  client.setQueryData(queries.boards().queryKey, state.boards());
  client.setQueryData(
    queries.openAsks().queryKey,
    state.asks(new URLSearchParams({ status: "open" })),
  );
  client.setQueryData(queries.activeTasks().queryKey, active());
  client.setQueryData(queries.agents().queryKey, state.agents(none));
  client.setQueryData(queries.updates("").queryKey, state.updates(none));
}

describe("inProgress", () => {
  test("lists doing then review tasks, newest update first, without closed boards", () => {
    const tasks = active();
    const listed = inProgress(tasks, new Set());
    expect(listed.length).toBe(tasks.length);
    const firstReview = listed.findIndex((task) => task.status === "review");
    expect(listed.slice(0, firstReview).every((task) => task.status === "doing")).toBe(true);
    expect(listed.slice(firstReview).every((task) => task.status === "review")).toBe(true);
    const board = tasks[0]?.board ?? "";
    expect(inProgress(tasks, new Set([board])).some((task) => task.board === board)).toBe(false);
  });
});

describe("home", () => {
  test("shows waiting asks, tasks in progress, updates and boards, in that order", async () => {
    await renderInApp(<HomeScreen />, { seed });
    const titles = screen
      .getAllByRole("heading", { level: 2 })
      .map((heading) => heading.textContent?.replace(/\d+$/, ""));
    expect(titles).toEqual(["Waiting on you", "In progress", "Since you last looked", "Boards"]);
    const progress = screen.getByRole("region", { name: /In progress/ });
    for (const task of active()) {
      expect(within(progress).getByText(task.title)).toBeTruthy();
    }
  });
});
