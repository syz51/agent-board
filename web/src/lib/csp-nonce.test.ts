import { afterEach, describe, expect, test } from "bun:test";
import { getNonce, setNonce } from "get-nonce";
import { cspNonce, installCspNonce } from "./csp-nonce.ts";

function page(nonce?: string): Document {
  const doc = document.implementation.createHTMLDocument("page");
  if (nonce !== undefined) {
    const meta = doc.createElement("meta");
    meta.name = "csp-nonce";
    meta.content = nonce;
    doc.head.append(meta);
  }
  return doc;
}

describe("installCspNonce", () => {
  afterEach(() => setNonce(""));

  test("hands the page's nonce to Radix and to nonce props", () => {
    installCspNonce(page("0f3a"));
    expect(cspNonce()).toBe("0f3a");
    expect(getNonce()).toBe("0f3a");
  });

  test("leaves styles without a nonce when the page has none", () => {
    installCspNonce(page());
    expect(cspNonce()).toBeUndefined();
    installCspNonce(page(""));
    expect(cspNonce()).toBeUndefined();
  });
});
