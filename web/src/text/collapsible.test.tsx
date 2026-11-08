import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import { HOSTILE_SAID, LONG_SAID } from "../../dev/fixtures.ts";
import { CollapsibleMarkdown, isLong } from "./collapsible.tsx";

const render = (text: string) => renderToStaticMarkup(<CollapsibleMarkdown text={text} />);

describe("collapsible agent markdown", () => {
  test("long text starts collapsed with a button to show all", () => {
    expect(isLong(LONG_SAID)).toBe(true);
    const html = render(LONG_SAID);
    expect(html).toContain("data-clamped");
    expect(html).toContain('aria-expanded="false"');
    expect(html).toContain("Show all");
  });

  test("short text shows in full with no button", () => {
    expect(isLong("Round done.")).toBe(false);
    const html = render("Round done.");
    expect(html).not.toContain("data-clamped");
    expect(html).not.toContain("<button");
  });

  test("agent text stays inert: no script, no javascript: link, no remote image", () => {
    expect(HOSTILE_SAID).toContain("<script>");
    const html = render(HOSTILE_SAID);
    expect(html).not.toContain("<script");
    expect(html).not.toContain("<img");
    expect(html).not.toContain('href="javascript:');
  });
});
