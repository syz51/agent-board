import { describe, expect, test } from "bun:test";
import { fireEvent, render, screen } from "@testing-library/react";
import type { Agent } from "../api/schema.ts";
import { DevState } from "../../dev/state.ts";
import { renderInApp } from "../../test/render.tsx";
import { agentAnchor, agentIndex, threadLink, Who } from "./common.tsx";
import { ExpandableLine } from "./common.tsx";

const state = new DevState(() => Date.parse("2026-11-06T12:00:00Z"));
const agents = state.agents(new URLSearchParams());
const index = agentIndex(agents);
const withLink = (link: string): Agent => ({
  ...agents[0]!,
  native_ids: { t3_thread: "thread-1", t3_link: link },
});

describe("threadLink", () => {
  test("takes only an https link", () => {
    expect(threadLink(withLink("https://t3.example/threads/env-1/thread-1"))).toBe(
      "https://t3.example/threads/env-1/thread-1",
    );
    expect(threadLink(withLink("http://t3.example/threads/env-1/thread-1"))).toBeNull();
    expect(threadLink(withLink("javascript:alert(1)"))).toBeNull();
    expect(threadLink(withLink("not a url"))).toBeNull();
    expect(threadLink({ ...agents[0]!, native_ids: {} })).toBeNull();
    expect(threadLink(undefined)).toBeNull();
  });
});

describe("Who", () => {
  test("links an agent with a thread link to its T3 thread, in a new tab", async () => {
    const agent = agents.find((item) => item.native_ids.t3_link)!;
    await renderInApp(<Who id={agent.id} agents={index} />);
    const link = screen.getByRole("link", { name: new RegExp(agent.handle) });
    expect(link.getAttribute("href")).toBe(agent.native_ids.t3_link ?? "");
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noopener noreferrer");
    expect(link.getAttribute("title")).toContain(agent.title ?? "");
  });

  test("links an agent without one to its card on the Agents page", async () => {
    const agent = agents.find((item) => !item.native_ids.t3_link && item.state !== "ended")!;
    await renderInApp(<Who id={agent.id} agents={index} />);
    const link = screen.getByRole("link", { name: new RegExp(agent.handle) });
    expect(link.getAttribute("href")).toBe(`/agents#${agentAnchor(agent.handle)}`);
  });

  test("shows the thread title under the handle when asked", async () => {
    const agent = agents.find((item) => item.title)!;
    await renderInApp(<Who id={agent.id} agents={index} withTitle />);
    expect(screen.getByText(agent.title!)).toBeTruthy();
  });

  test("an unknown agent and the user are plain text", async () => {
    await renderInApp(
      <>
        <Who id="t3:unknown" agents={index} />
        <Who id="user" agents={index} />
      </>,
    );
    expect(screen.queryByRole("link")).toBeNull();
    expect(screen.getByText("you")).toBeTruthy();
  });
});

// ErrorNotice is covered through the whole app, with the connection banner, in outage.test.tsx.

describe("ExpandableLine", () => {
  test("cuts to the lines asked for, and shows the whole text when pressed", () => {
    render(<ExpandableLine text="A long goal" lines={1} />);
    const line = screen.getByRole("button", { name: "A long goal" });
    expect(line.className).toContain("line-clamp-1");
    fireEvent.click(line);
    expect(line.getAttribute("aria-expanded")).toBe("true");
    expect(line.className).not.toContain("line-clamp");
  });
});
