import { afterEach, describe, expect, test } from "bun:test";
import type { QueryClient } from "@tanstack/react-query";
import { screen, within } from "@testing-library/react";
import { DevState } from "../../dev/state.ts";
import { renderInApp } from "../../test/render.tsx";
import { queries } from "../live/query.ts";
import { HOME_ROWS, HomeScreen, inProgress } from "./boards.tsx";

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
  test("shows waiting asks, boards, tasks in progress and updates, in that order", async () => {
    await renderInApp(<HomeScreen />, { seed });
    const titles = screen
      .getAllByRole("heading", { level: 2 })
      .map((heading) => heading.textContent?.replace(/\d.*$/, ""));
    expect(titles).toEqual(["Needs you", "Boards", "In progress", "Since you last looked"]);
  });

  test("lists every task in progress, so its counts match its rows", async () => {
    await renderInApp(<HomeScreen />, { seed });
    const progress = screen.getByRole("region", { name: /In progress/ });
    const listed = inProgress(active(), new Set());
    for (const task of listed) {
      expect(within(progress).getByText(task.title)).toBeTruthy();
    }
    const heading = screen.getByRole("heading", { level: 2, name: /In progress/ });
    expect(heading.textContent).toBe(`In progress${listed.length}`);
    expect(within(progress).queryByRole("button", { name: /^Show/ })).toBeNull();
  });

  test("marks each waiting ask's row with its kind's colour and lists at most four", async () => {
    await renderInApp(<HomeScreen />, { seed });
    const asks = screen.getByRole("region", { name: /Needs you/ });
    const rows = within(asks)
      .getAllByRole("link")
      .filter((link) => link.closest("li"));
    const open = state.asks(new URLSearchParams({ status: "open" }));
    expect(rows.length).toBe(Math.min(open.length, HOME_ROWS));
    for (const row of rows) {
      const ask = open.find((item) => row.getAttribute("href") === `/asks/${item.id}`)!;
      expect(row.className).toContain(
        ask.kind === "action" ? "before:bg-amber" : "before:bg-status-review",
      );
    }
  });

  test("splits tasks in progress into Doing and Review, each under its own heading", async () => {
    await renderInApp(<HomeScreen />, { seed });
    const progress = screen.getByRole("region", { name: /In progress/ });
    const groups = within(progress)
      .getAllByRole("heading", { level: 3 })
      .map((heading) => heading.textContent);
    const tasks = active();
    const doing = tasks.filter((task) => task.status === "doing");
    const review = tasks.filter((task) => task.status === "review");
    expect(doing.length).toBeGreaterThan(0);
    expect(review.length).toBeGreaterThan(0);
    expect(groups).toEqual([`Doing${doing.length}`, `Review${review.length}`]);
    const reviewGroup = within(progress).getByRole("region", { name: /^Review/ });
    for (const task of review) {
      expect(within(reviewGroup).getByText(task.title)).toBeTruthy();
    }
    for (const task of doing) {
      expect(within(reviewGroup).queryByText(task.title)).toBeNull();
    }
  });
});
