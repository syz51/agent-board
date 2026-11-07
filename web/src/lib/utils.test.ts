import { describe, expect, test } from "bun:test";
import { cn } from "./utils.ts";

describe("cn", () => {
  test("keeps a theme font size beside a text colour", () => {
    expect(cn("text-meta text-subtle-foreground")).toBe("text-meta text-subtle-foreground");
    expect(cn("text-sm", "text-meta")).toBe("text-meta");
    expect(cn("text-display", "max-md:text-display-phone")).toBe(
      "text-display max-md:text-display-phone",
    );
  });

  test("lets a theme shadow replace another shadow", () => {
    expect(cn("shadow-xs", "shadow-raise")).toBe("shadow-raise");
  });
});
