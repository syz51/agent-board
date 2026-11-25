import { describe, expect, test } from "vite-plus/test";
import { parseSearch, stringifySearch, validateBoardSearch } from "./router.tsx";

describe("search params", () => {
  test("board filters keep known non-empty values and drop the rest", () => {
    expect(
      validateBoardSearch({
        q: "lease",
        priority: "p1",
        owner: "none",
        ready: "1",
        parent: "factory-12",
        tab: "proposals",
        x: "y",
      }),
    ).toEqual({
      q: "lease",
      priority: "p1",
      owner: "none",
      ready: "1",
      parent: "factory-12",
      tab: "proposals",
    });
    expect(validateBoardSearch({ q: "", ready: "true", priority: 3, tab: "up-next" })).toEqual({
      q: undefined,
      priority: undefined,
      owner: undefined,
      ready: undefined,
      parent: undefined,
      tab: undefined,
    });
    expect(validateBoardSearch({ tab: "icebox" }).tab).toBe("icebox");
    expect(validateBoardSearch({ tab: "backlog" }).tab).toBe("backlog");
    expect(validateBoardSearch({ tab: "unknown" }).tab).toBeUndefined();
  });

  test("stay plain query strings, without JSON quoting or empty values", () => {
    expect(stringifySearch({ q: "a b", ready: "1", owner: undefined, priority: "" })).toBe(
      "?q=a+b&ready=1",
    );
    expect(stringifySearch({})).toBe("");
    expect(parseSearch("?q=a+b&ready=1")).toEqual({ q: "a b", ready: "1" });
    expect(parseSearch(stringifySearch({ board: "web-view" }))).toEqual({ board: "web-view" });
  });
});
