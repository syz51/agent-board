// Agent-written text is untrusted. Markdown is parsed into tokens with `marked`'s lexer and
// rendered as Preact elements here; no HTML string is ever produced or injected, so raw
// HTML in the source shows as literal text. Links work only for http, https and mailto
// and always show their target; images are never loaded.

import { Lexer, type Token, type Tokens } from "marked";
import type { ComponentChildren, JSX } from "preact";

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

const ENTITIES: Record<string, string> = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: " " };

/** Markdown text keeps entity references as written; show them as the characters. */
function decodeEntities(text: string): string {
  return text.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (match, name: string) => {
    if (name[0] === "#") {
      const code = name[1] === "x" || name[1] === "X"
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
  return <div class="md">{blocks(tokens)}</div>;
}

/** Agent text shown as written, keeping line breaks. */
export function PlainText({ text, class: className }: { text: string; class?: string }) {
  return <span class={className ? `plain ${className}` : "plain"}>{text}</span>;
}

function blocks(tokens: Token[]): ComponentChildren[] {
  return tokens.map((token, index) => block(token, index));
}

function block(token: Token, key: number): ComponentChildren {
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
      return <Tag key={key} class="md-heading">{inline(tokens)}</Tag>;
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
      return <ol key={key} start={start}>{items}</ol>;
    }
    case "hr":
      return <hr key={key} />;
    case "table": {
      const table = token as Tokens.Table;
      return (
        <div key={key} class="md-table">
          <table>
            <thead>
              <tr>{table.header.map((cell, i) => <th key={i}>{inline(cell.tokens)}</th>)}</tr>
            </thead>
            <tbody>
              {table.rows.map((row, r) => (
                <tr key={r}>{row.map((cell, i) => <td key={i}>{inline(cell.tokens)}</td>)}</tr>
              ))}
            </tbody>
          </table>
        </div>
      );
    }
    case "html":
      // Raw HTML is shown as the text it is.
      return <p key={key} class="md-raw">{(token as Tokens.HTML).text.trimEnd()}</p>;
    default:
      return <p key={key}>{t.raw}</p>;
  }
}

function listItem(item: Tokens.ListItem, loose: boolean): ComponentChildren[] {
  return item.tokens.map((token, index) => {
    if (token.type === "checkbox") {
      const { checked } = token as Tokens.Checkbox;
      return (
        <span key={index} class="md-check" role="img" aria-label={checked ? "done" : "not done"}>
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

function inline(tokens: Token[] | undefined): ComponentChildren[] {
  return (tokens ?? []).map((token, key) => inlineToken(token, key));
}

function inlineToken(token: Token, key: number): ComponentChildren {
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
    case "codespan":
      return <code key={key}>{(token as Tokens.Codespan).text}</code>;
    case "br":
      return <br key={key} />;
    case "link": {
      const link = token as Tokens.Link;
      return <SafeLink key={key} href={link.href}>{inline(link.tokens)}</SafeLink>;
    }
    case "image": {
      const image = token as Tokens.Image;
      const label = image.text ? `image: ${image.text}` : "image";
      return <SafeLink key={key} href={image.href}>{label}</SafeLink>;
    }
    case "html":
      return <code key={key} class="md-raw">{(token as Tokens.Tag).text}</code>;
    default:
      return (token as Tokens.Generic).raw;
  }
}

/** A link that shows where it goes. Unsafe targets are shown, not linked. */
export function SafeLink(
  { href, children }: { href: string; children: ComponentChildren },
): JSX.Element {
  const url = safeHref(href);
  if (url === null) {
    return (
      <span class="md-link-blocked">
        {children} <span class="link-target">(link not followed: {href})</span>
      </span>
    );
  }
  const shown = url.startsWith("mailto:") ? url.slice(7) : url;
  const childText = typeof children === "string" ? children : flatText(children);
  const showTarget = childText !== href && childText !== url && childText !== shown;
  return (
    <span class="md-link">
      <a href={url} target="_blank" rel="noopener noreferrer nofollow" referrerpolicy="no-referrer">
        {children}
      </a>
      {showTarget && <span class="link-target"> ({shown})</span>}
    </span>
  );
}

function flatText(children: ComponentChildren): string {
  if (typeof children === "string" || typeof children === "number") return String(children);
  if (Array.isArray(children)) return children.map(flatText).join("");
  return "\u0000";
}
