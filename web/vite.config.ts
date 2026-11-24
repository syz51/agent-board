// Vite+ configuration for the agent-board web view. `vp build` writes the production
// build to `dist/`, which board-server embeds and serves at `/` with an SPA fallback; the dev
// server (dev/server.ts) builds the same way into a cache directory. Asset URLs are absolute
// (`/assets/...`) so they resolve from any route, such as `/tasks/factory-12`.

import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig, type Plugin } from "vite-plus";

// Where the style nonce goes in index.html. board-server (crates/board-server/src/web.rs)
// and the dev server replace it with a fresh nonce on every page response. Braces are not
// valid in a nonce, so a page served without the replacement allows no `<style>` at all.
export const NONCE_PLACEHOLDER = "{{CSP_NONCE}}";

// Defence in depth for agent-written text: only this origin's scripts, styles and API, no
// remote images. `<style>` elements need the page's nonce: Radix adds them for its scroll
// lock and Select, and src/lib/csp-nonce.ts hands them the nonce from the `csp-nonce` meta,
// whose `nonce` attribute the browser hides from selectors and `getAttribute`.
// The server sends the same policy as a header, which also allows `frame-ancestors`.
const CSP = [
  "default-src 'self'",
  "script-src 'self'",
  `style-src 'self' 'nonce-${NONCE_PLACEHOLDER}'`,
  "img-src 'self' data:",
  "connect-src 'self'",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'self'",
].join("; ");

function cspNoncePlugin(): Plugin {
  return {
    name: "csp-nonce-meta",
    transformIndexHtml(html) {
      const meta =
        `<meta http-equiv="Content-Security-Policy" content="${CSP}" />\n    ` +
        `<meta property="csp-nonce" nonce="${NONCE_PLACEHOLDER}" />`;
      if (!html.includes('<meta charset="utf-8" />')) {
        throw new Error("index.html lost its charset meta");
      }
      return html.replace('<meta charset="utf-8" />', `<meta charset="utf-8" />\n    ${meta}`);
    },
  };
}

export default defineConfig({
  base: "/",
  plugins: [react(), tailwindcss(), cspNoncePlugin()],
  resolve: {
    alias: {
      "@": fileURLToPath(new URL("./src", import.meta.url)),
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Keep font and CSS assets as same-origin files rather than data: URLs (`font-src 'self'`).
    assetsInlineLimit: 0,
    // The static SPA embeds as a single bundle (~190 KB gzipped) into board-server.
    chunkSizeWarningLimit: 700,
  },
  lint: {
    ignorePatterns: ["dist/**"],
    plugins: ["typescript", "react", "oxc"],
    categories: {
      correctness: "error",
    },
    options: {
      typeAware: true,
      typeCheck: true,
    },
    rules: {
      "eslint/no-unused-vars": ["error", { argsIgnorePattern: "^_", varsIgnorePattern: "^_" }],
      "typescript/consistent-type-imports": "error",
      "react/rules-of-hooks": "error",
      "react/exhaustive-deps": "warn",
    },
  },
  fmt: {
    ignorePatterns: ["dist/**", "bun.lock", "ui-climb.json", "*.md"],
    printWidth: 100,
    sortTailwindcss: {
      stylesheet: "./src/styles.css",
      functions: ["cn", "cva"],
    },
  },
  test: {
    pool: "threads",
    environment: "happy-dom",
    setupFiles: ["./test/setup.ts"],
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
