// The page's style nonce. The CSP allows `<style>` elements only with it; the server puts a
// fresh one on the `csp-nonce` meta of every page (see build.ts), in a `nonce` attribute,
// which the browser hides from selectors and `getAttribute` but keeps in `.nonce`. Radix's
// scroll lock (Dialog, Sheet, Select, menus) takes it through get-nonce; pass `cspNonce()`
// as the `nonce` prop of Radix `Select.Viewport` and `ScrollArea.Viewport`, which render
// their own `<style>`.

import { setNonce } from "get-nonce";

let nonce: string | undefined;

/** Read the nonce from `doc` and hand it to Radix. Call once, before the first render. */
export function installCspNonce(doc: Document = document): void {
  const meta = doc.querySelector<HTMLMetaElement>('meta[property="csp-nonce"]');
  nonce = meta?.nonce || meta?.getAttribute("nonce") || undefined;
  setNonce(nonce ?? "");
}

/** The page's style nonce, for components that take a `nonce` prop. */
export function cspNonce(): string | undefined {
  return nonce;
}
