// Agent-written text is untrusted. Markdown is parsed into tokens with `marked`'s lexer and
// rendered as React elements here; no HTML string is ever produced or injected, so raw
// HTML in the source shows as literal text. Links work only for http, https and mailto,
// and for T3 thread links that map to T3's https page; they always show their target.
// Images are never loaded.

import { Lexer, type Token, type Tokens } from "marked";
import { type ReactElement, type ReactNode, useContext } from "react";
import { cn } from "@/lib/utils.ts";
import { ThreadBasesContext, threadUrl } from "./thread-links.ts";

const SAFE_PROTOCOLS = new Set(["http:", "https:", "mailto:"]);

/** The URL to use for `href`, or null when the target is not an absolute safe URL. */
export function safeHref(href: string): string | null {
  let url: URL;
  try {
    url = new URL(href.trim());
  } catch {
    return null;
  }
  return SAFE_PROTOCOLS.has(url.protocol) ? url.href : null;
}

const ENTITIES: Record<string, string> = {
  amp: "&",
  lt: "<",
  gt: ">",
  quot: '"',
  apos: "'",
  nbsp: " ",
};

/** Markdown text keeps entity references as written; show them as the characters. */
function decodeEntities(text: string): string {
  return text.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (match, name: string) => {
    if (name[0] === "#") {
      const code =
        name[1] === "x" || name[1] === "X"
          ? Number.parseInt(name.slice(2), 16)
          : Number.parseInt(name.slice(1), 10);
      return code > 0 && code <= 0x10ffff ? String.fromCodePoint(code) : match;
    }
    return ENTITIES[name.toLowerCase()] ?? match;
  });
}

export function Markdown({ text }: { text: string }) {
  if (!text.trim()) return null;
  const tokens = new Lexer({ gfm: true, breaks: true }).lex(text);
  return <div className="md">{blocks(tokens)}</div>;
}

/** An HTML-like tag in agent text, such as `</board>` or `<b class="x">`. */
const TAG = /(<\/?[A-Za-z][^<>]*>)/;

/**
 * Agent text shown as written, keeping line breaks. Tags in it show as code, as raw HTML
 * does in agent markdown, so markup reads as inert text and not as the page's own.
 */
export function PlainText({ text, className }: { text: string; className?: string }) {
  const parts = text.split(TAG);
  return (
    <span className={cn("whitespace-pre-wrap", className)}>
      {parts.length === 1
        ? text
        : parts.map((part, index) =>
            // split() with a capturing group puts the tags at odd indexes.
            index % 2 === 1 ? (
              <code key={index} className="raw-tag">
                {part}
              </code>
            ) : (
              part
            ),
          )}
    </span>
  );
}

/** Short inline code stays whole on one line; the line breaks between chips instead. */
const CHIP_MAX = 32;

function chip(text: string): string | undefined {
  return text.length <= CHIP_MAX ? "md-chip" : undefined;
}

function blocks(tokens: Token[]): ReactNode[] {
  return tokens.map((token, index) => block(token, index));
}

function block(token: Token, key: number): ReactNode {
  const t = token as Tokens.Generic;
  switch (token.type) {
    case "space":
    case "def":
      return null;
    case "heading": {
      const { depth, tokens } = token as Tokens.Heading;
      // The page owns h1 and h2; agent headings start at h3.
      const level = Math.min(depth + 2, 6);
      const Tag = `h${level}` as "h3" | "h4" | "h5" | "h6";
      return (
        <Tag key={key} className="md-heading">
          {inline(tokens)}
        </Tag>
      );
    }
    case "paragraph":
      return <p key={key}>{inline((token as Tokens.Paragraph).tokens)}</p>;
    case "text": {
      const text = token as Tokens.Text;
      return <p key={key}>{text.tokens ? inline(text.tokens) : decodeEntities(text.text)}</p>;
    }
    case "code":
      return (
        <pre key={key}>
          <code>{(token as Tokens.Code).text}</code>
        </pre>
      );
    case "blockquote":
      return <blockquote key={key}>{blocks((token as Tokens.Blockquote).tokens)}</blockquote>;
    case "list": {
      const list = token as Tokens.List;
      const items = list.items.map((item, index) => (
        <li key={index}>{listItem(item, list.loose)}</li>
      ));
      if (!list.ordered) return <ul key={key}>{items}</ul>;
      const start = typeof list.start === "number" ? list.start : 1;
      return (
        <ol key={key} start={start}>
          {items}
        </ol>
      );
    }
    case "hr":
      return <hr key={key} />;
    case "table": {
      const table = token as Tokens.Table;
      return (
        <div key={key} className="md-table">
          <table>
            <thead>
              <tr>
                {table.header.map((cell, i) => (
                  <th key={i}>{inline(cell.tokens)}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {table.rows.map((row, r) => (
                <tr key={r}>
                  {row.map((cell, i) => (
                    <td key={i}>{inline(cell.tokens)}</td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    }
    case "html":
      // Raw HTML is shown as the text it is.
      return (
        <p key={key} className="md-raw">
          {(token as Tokens.HTML).text.trimEnd()}
        </p>
      );
    default:
      return <p key={key}>{t.raw}</p>;
  }
}

function listItem(item: Tokens.ListItem, loose: boolean): ReactNode[] {
  return item.tokens.map((token, index) => {
    if (token.type === "checkbox") {
      const { checked } = token as Tokens.Checkbox;
      return (
        <span
          key={index}
          className="md-check"
          role="img"
          aria-label={checked ? "done" : "not done"}
        >
          {checked ? "☑ " : "☐ "}
        </span>
      );
    }
    if (token.type === "text" && !loose) {
      const text = token as Tokens.Text;
      return text.tokens ? inline(text.tokens) : decodeEntities(text.text);
    }
    return block(token, index);
  });
}

function inline(tokens: Token[] | undefined): ReactNode[] {
  return (tokens ?? []).map((token, key) => inlineToken(token, key));
}

function inlineToken(token: Token, key: number): ReactNode {
  switch (token.type) {
    case "text": {
      const text = token as Tokens.Text;
      return text.tokens ? inline(text.tokens) : decodeEntities(text.text);
    }
    case "escape":
      return (token as Tokens.Escape).text;
    case "strong":
      return <strong key={key}>{inline((token as Tokens.Strong).tokens)}</strong>;
    case "em":
      return <em key={key}>{inline((token as Tokens.Em).tokens)}</em>;
    case "del":
      return <del key={key}>{inline((token as Tokens.Del).tokens)}</del>;
    case "codespan": {
      const { text } = token as Tokens.Codespan;
      return (
        <code key={key} className={chip(text)}>
          {text}
        </code>
      );
    }
    case "br":
      return <br key={key} />;
    case "link": {
      const link = token as Tokens.Link;
      return (
        <SafeLink key={key} href={link.href}>
          {inline(link.tokens)}
        </SafeLink>
      );
    }
    case "image": {
      const image = token as Tokens.Image;
      const label = image.text ? `image: ${image.text}` : "image";
      return (
        <SafeLink key={key} href={image.href}>
          {label}
        </SafeLink>
      );
    }
    case "html":
      return (
        <code key={key} className={cn("md-raw", chip((token as Tokens.Tag).text))}>
          {(token as Tokens.Tag).text}
        </code>
      );
    default:
      return (token as Tokens.Generic).raw;
  }
}

/**
 * A link that shows where it goes: the host after its text (the whole URL in a tooltip), so
 * link text cannot hide another site. Unsafe targets are shown, not linked.
 */
export function SafeLink({ href, children }: { href: string; children: ReactNode }): ReactElement {
  const bases = useContext(ThreadBasesContext);
  const direct = safeHref(href);
  // A T3 thread link opens as T3's https page; its text names another target, so show it.
  const url = direct ?? threadUrl(href, bases);
  const childText = typeof children === "string" ? children : flatText(children);
  if (url === null) {
    // An autolink such as <t3-thread://...> already shows its target as its text.
    return (
      <span className="md-link-blocked text-subtle-foreground">
        {children}{" "}
        <span className="link-target inline-block text-meta text-muted-foreground">
          {childText === href ? "(link not followed)" : `(link not followed: ${href})`}
        </span>
      </span>
    );
  }
  const shown = url.startsWith("mailto:") ? url.slice(7) : url;
  const showTarget =
    direct === null || (childText !== href && childText !== url && childText !== shown);
  const target = url.startsWith("mailto:") ? shown : new URL(url).host;
  return (
    <span className="md-link">
      <a href={url} target="_blank" rel="noopener noreferrer nofollow" referrerPolicy="no-referrer">
        {children}
      </a>
      {showTarget && (
        <span className="link-target text-meta text-muted-foreground" title={url}>
          {" "}
          ({target})
        </span>
      )}
    </span>
  );
}

function flatText(children: ReactNode): string {
  if (typeof children === "string" || typeof children === "number") return String(children);
  if (Array.isArray(children)) return children.map(flatText).join("");
  return "\u0000";
}
