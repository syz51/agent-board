// The page's style nonce. The CSP allows `<style>` elements only with it; the server puts a
// fresh one in the `csp-nonce` meta of every page (see build.ts). Radix's scroll lock (Dialog,
// Sheet, Select, menus) takes it through get-nonce; pass `cspNonce()` as the `nonce` prop of
// Radix Select and ScrollArea, which render their own `<style>`.

import { setNonce } from "get-nonce";

let nonce: string | undefined;

/** Read the nonce from `doc` and hand it to Radix. Call once, before the first render. */
export function installCspNonce(doc: Document = document): void {
  nonce = doc.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content || undefined;
  if (nonce) setNonce(nonce);
}

/** The page's style nonce, for components that take a `nonce` prop. */
export function cspNonce(): string | undefined {
  return nonce;
}
