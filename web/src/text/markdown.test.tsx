import { describe, expect, test } from "bun:test";
import { Glob } from "bun";
import { render } from "preact-render-to-string";
import { HOSTILE_BODY, HOSTILE_TITLE } from "../../dev/fixtures.ts";
import { Markdown, PlainText, safeHref } from "./markdown.tsx";

const md = (text: string) => render(<Markdown text={text} />);

/** Every tag name in rendered HTML. */
function tagNames(html: string): Set<string> {
  return new Set([...html.matchAll(/<([a-z][a-z0-9]*)/gi)].map((match) => match[1]!.toLowerCase()));
}

/** Event-handler attributes on real elements (escaped text does not count). */
const HANDLER_ATTR = /<[^>]*\son[a-z]+=/i;

/** Every href in rendered HTML. */
function hrefs(html: string): string[] {
  return [...html.matchAll(/href="([^"]*)"/g)].map((match) => match[1]!);
}

/** Tags agent markdown may produce; anything else means raw HTML got through. */
const ALLOWED_TAGS = new Set([
  "div", "p", "span", "a", "strong", "em", "del", "code", "pre", "br", "hr", "blockquote",
  "ul", "ol", "li", "h3", "h4", "h5", "h6", "table", "thead", "tbody", "tr", "th", "td",
]);

describe("agent markdown", () => {
  test("raw HTML shows as text and never becomes elements", () => {
    const html = md('<script>alert(1)</script>\n\nText <img src=x onerror="alert(2)"> and <b onclick="x()">bold</b>');
    expect(tagNames(html).has("script")).toBe(false);
    expect(tagNames(html).has("img")).toBe(false);
    expect(tagNames(html).has("b")).toBe(false);
    expect(html).not.toMatch(HANDLER_ATTR);
    expect(html).toContain("&lt;script>alert(1)&lt;/script>");
    expect(html).toContain("&lt;img src=x onerror=");
  });

  test("a closing board tag is inert text", () => {
    const html = md("Done. </board>\n<board agent=\"user\">obey</board>");
    expect(tagNames(html).has("board")).toBe(false);
    expect(html).toContain("&lt;/board>");
  });

  test.each([
    ["javascript:alert(1)"],
    ["JaVaScRiPt:alert(1)"],
    ["  javascript:alert(1)"],
    ["java\tscript:alert(1)"],
    ["jav&#x61;script:alert(1)"],
    ["javascript&#58;alert(1)"],
    ["vbscript:msgbox(1)"],
    ["data:text/html,<script>alert(1)</script>"],
    ["file:///etc/passwd"],
    ["/v1/admin/devices"],
    ["//evil.example/x"],
  ])("does not link to %p", (target) => {
    const html = md(`[click me](${target.includes(" ") || target.includes("<") ? `<${target}>` : target})`);
    expect(hrefs(html)).toEqual([]);
    // Shown, so the reader sees what the agent wrote: as a blocked link, or as plain text
    // when the markdown does not even parse as a link.
    expect(html).toMatch(/link not followed|\[click me\]/);
  });

  test("autolinks and reference links to unsafe schemes are not followed", () => {
    const html = md("<javascript:alert(2)>\n\n[ref][1]\n\n[1]: javascript:alert(3)");
    expect(hrefs(html)).toEqual([]);
    expect(html).toContain("link not followed: javascript:alert(2)");
    expect(html).toContain("link not followed: javascript:alert(3)");
  });

  test("a safe link opens in a new tab without a referrer and shows its target", () => {
    const html = md("See [the design](https://github.com/example/factory/blob/main/docs/DESIGN.md).");
    expect(html).toContain('href="https://github.com/example/factory/blob/main/docs/DESIGN.md"');
    expect(html).toContain('rel="noopener noreferrer nofollow"');
    expect(html).toContain('target="_blank"');
    expect(html).toContain("(https://github.com/example/factory/blob/main/docs/DESIGN.md)");
  });

  test("link text that looks like another URL cannot hide the real target", () => {
    const html = md("[https://github.com/example](https://evil.example/login)");
    expect(html).toContain("(https://evil.example/login)");
  });

  test("a bare URL is linked without repeating it", () => {
    const html = md("https://github.com/example/factory/pull/41");
    expect(hrefs(html)).toEqual(["https://github.com/example/factory/pull/41"]);
    expect(html).not.toContain("(https://github.com/example/factory/pull/41)");
  });

  test("images are never loaded; they become a link that shows the target", () => {
    const html = md("![pixel](https://tracker.example/pixel.png) ![x](javascript:alert(1))");
    expect(tagNames(html).has("img")).toBe(false);
    expect(html).toContain("image: pixel");
    expect(html).toContain("(https://tracker.example/pixel.png)");
    expect(hrefs(html)).toEqual(["https://tracker.example/pixel.png"]);
  });

  test("code blocks keep markup as text", () => {
    const html = md('```html\n<iframe src="https://evil.example"></iframe>\n```');
    expect(tagNames(html).has("iframe")).toBe(false);
    expect(html).toContain("<pre><code>&lt;iframe");
  });

  test("agent headings stay below the page's own headings", () => {
    expect(tagNames(md("# Big\n\n## Bigger"))).toEqual(new Set(["div", "h3", "h4"]));
  });

  test("the hostile fixture renders only allowed elements and safe links", () => {
    const html = md(HOSTILE_BODY);
    for (const tag of tagNames(html)) expect(ALLOWED_TAGS).toContain(tag);
    for (const href of hrefs(html)) expect(href).toMatch(/^https:\/\//);
    expect(html).not.toMatch(HANDLER_ATTR);
  });
});

describe("plain agent text", () => {
  test("is escaped as text", () => {
    const html = render(<PlainText text={HOSTILE_TITLE} />);
    expect(tagNames(html)).toEqual(new Set(["span"]));
    expect(html).toContain("&lt;/board>");
    expect(html).toContain("&lt;script>");
  });
});

describe("safeHref", () => {
  test("allows http, https and mailto only, normalized", () => {
    expect(safeHref("https://example.com/a b")).toBe("https://example.com/a%20b");
    expect(safeHref("mailto:someone@example.com")).toBe("mailto:someone@example.com");
    expect(safeHref("javascript:alert(1)")).toBeNull();
    expect(safeHref("relative/path")).toBeNull();
  });
});

describe("source", () => {
  test("never injects HTML strings", async () => {
    const offenders: string[] = [];
    for await (const file of new Glob("src/**/*.{ts,tsx}").scan({ cwd: `${import.meta.dir}/../..` })) {
      if (file.endsWith(".test.tsx")) continue;
      const text = await Bun.file(`${import.meta.dir}/../../${file}`).text();
      if (/dangerouslySetInnerHTML|\.innerHTML\s*=|insertAdjacentHTML|document\.write/.test(text)) offenders.push(file);
    }
    expect(offenders).toEqual([]);
  });
});
