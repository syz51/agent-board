import { describe, expect, test } from "bun:test";
import type { Agent } from "@/api/schema.ts";
import { threadBases, threadUrl } from "./thread-links.ts";

const withLink = (t3_link?: string): Agent =>
  ({ native_ids: t3_link === undefined ? {} : { t3_link } }) as Agent;

const BASES = threadBases([withLink("https://t3.example/threads/env-1/thread-1")]);

describe("threadBases", () => {
  test("reads each environment's base from an https t3_link", () => {
    const bases = threadBases([
      withLink(),
      withLink("https://t3.example/threads/env-1/thread%3Aone"),
      withLink("https://t3.example/app/threads/env-2/thread-2"),
      withLink("https://other.example/threads/env-1/thread-3"),
    ]);
    expect([...bases]).toEqual([
      ["env-1", "https://t3.example/threads/"],
      ["env-2", "https://t3.example/app/threads/"],
    ]);
  });

  test.each([
    ["http://t3.example/threads/env-1/thread-1"],
    ["javascript:alert(1)"],
    ["not a url"],
    ["https://t3.example/thread/env-1/thread-1"],
    ["https://t3.example/threads/env-1"],
    ["https://t3.example/threads/env%2F1/thread-1"],
  ])("ignores %p", (link) => {
    expect(threadBases([withLink(link)]).size).toBe(0);
  });

  test("is empty without agents", () => {
    expect(threadBases(undefined).size).toBe(0);
  });
});

describe("threadUrl", () => {
  test("maps a thread link to the thread's https page", () => {
    expect(threadUrl("t3-thread://v2/env-1/mcp:abc", BASES)).toBe(
      "https://t3.example/threads/env-1/mcp%3Aabc",
    );
  });

  test("encodes the id as written, as T3's own links do", () => {
    expect(threadUrl("t3-thread://v2/env-1/thread:task:cmd%3Amcp%3Aabc", BASES)).toBe(
      "https://t3.example/threads/env-1/thread%3Atask%3Acmd%253Amcp%253Aabc",
    );
  });

  test.each([
    ["a launch id", "t3-thread://v2/env-1/mcp:launch:abc"],
    ["an unknown environment", "t3-thread://v2/env-9/mcp:abc"],
    ["another version", "t3-thread://v1/env-1/mcp:abc"],
    ["an extra segment", "t3-thread://v2/env-1/mcp:abc/more"],
    ["a query", "t3-thread://v2/env-1/mcp:abc?x=1"],
    ["a missing id", "t3-thread://v2/env-1/"],
    ["another scheme", "t3code://v2/env-1/mcp:abc"],
    ["plain text", "env-1 mcp:abc"],
  ])("leaves %s unmapped", (_, href) => {
    expect(threadUrl(href, BASES)).toBeNull();
  });

  test("maps nothing before the agents load", () => {
    expect(threadUrl("t3-thread://v2/env-1/mcp:abc", new Map())).toBeNull();
  });
});
