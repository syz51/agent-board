import { afterEach, describe, expect, test } from "bun:test";
import { getNonce, setNonce } from "get-nonce";
import { cspNonce, installCspNonce } from "./csp-nonce.ts";

function page(nonce?: string): Document {
  const doc = document.implementation.createHTMLDocument("page");
  if (nonce !== undefined) {
    const meta = doc.createElement("meta");
    meta.setAttribute("property", "csp-nonce");
    meta.setAttribute("nonce", nonce);
    doc.head.append(meta);
  }
  return doc;
}

describe("installCspNonce", () => {
  afterEach(() => installCspNonce(page()));

  test("hands the page's nonce to Radix and to nonce props", () => {
    installCspNonce(page("0f3a"));
    expect(cspNonce()).toBe("0f3a");
    expect(getNonce()).toBe("0f3a");
  });

  test("clears an earlier nonce when the page has none", () => {
    setNonce("stale");
    installCspNonce(page());
    expect(cspNonce()).toBeUndefined();
    expect(getNonce()).toBeFalsy();
    installCspNonce(page(""));
    expect(cspNonce()).toBeUndefined();
    expect(getNonce()).toBeFalsy();
  });
});
