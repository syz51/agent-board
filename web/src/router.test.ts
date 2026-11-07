import { describe, expect, test } from "bun:test";
import { parseSearch, stringifySearch, validateBoardSearch } from "./router.tsx";

describe("search params", () => {
  test("board filters keep known non-empty values and drop the rest", () => {
    expect(
      validateBoardSearch({ q: "lease", priority: "p1", owner: "none", ready: "1", x: "y" }),
    ).toEqual({
      q: "lease",
      priority: "p1",
      owner: "none",
      ready: "1",
    });
    expect(validateBoardSearch({ q: "", ready: "true", priority: 3 })).toEqual({
      q: undefined,
      priority: undefined,
      owner: undefined,
      ready: undefined,
    });
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
