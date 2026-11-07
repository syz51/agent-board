// Bundles index.html with its script and stylesheet. `bun run build` writes the production
// build to `dist/`, which board-server embeds and serves at `/` with an SPA fallback; the dev
// server (dev/server.ts) builds the same way into a cache directory. Asset URLs are absolute
// (`/assets/...`) so they resolve from any route, such as `/tasks/factory-12`.
//
// The dev server does not use Bun's HTML-import dev bundler: its module runtime cannot load
// @tanstack/router-core, whose router and route-loading modules import each other.

import { rm } from "node:fs/promises";
import tailwind from "bun-plugin-tailwind";

// Where the style nonce goes in index.html. board-server (crates/board-server/src/web.rs)
// and the dev server replace it with a fresh nonce on every page response.
export const NONCE_PLACEHOLDER = "__CSP_NONCE__";

// Defence in depth for agent-written text: only this origin's scripts, styles and API, no
// remote images. `<style>` elements need the page's nonce: Radix adds them for its scroll
// lock and Select, and src/lib/csp-nonce.ts hands them the nonce from the `csp-nonce` meta.
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

/** Build the web view into `outdir`, replacing what is there. Throws when the build fails. */
export async function buildApp({
  outdir,
  production,
}: {
  outdir: string;
  production: boolean;
}): Promise<Bun.BuildArtifact[]> {
  await rm(outdir, { recursive: true, force: true });
  const result = await Bun.build({
    entrypoints: [new URL("./index.html", import.meta.url).pathname],
    outdir,
    target: "browser",
    plugins: [tailwind],
    minify: production,
    sourcemap: production ? "none" : "linked",
    publicPath: "/",
    naming: {
      entry: "[name].[ext]",
      chunk: "assets/[name]-[hash].[ext]",
      asset: "assets/[name]-[hash].[ext]",
    },
    define: {
      "process.env.NODE_ENV": JSON.stringify(production ? "production" : "development"),
    },
    throw: false,
  });
  if (!result.success) {
    throw new AggregateError(result.logs, "The web view failed to build; see the errors above.");
  }

  const indexFile = Bun.file(`${outdir}/index.html`);
  const html = await indexFile.text();
  const meta =
    `<meta http-equiv="Content-Security-Policy" content="${CSP}" />\n    ` +
    `<meta name="csp-nonce" content="${NONCE_PLACEHOLDER}" />`;
  if (!html.includes('<meta charset="utf-8" />')) {
    throw new Error("index.html lost its charset meta");
  }
  await Bun.write(
    indexFile,
    html.replace('<meta charset="utf-8" />', `<meta charset="utf-8" />\n    ${meta}`),
  );
  return result.outputs;
}

if (import.meta.main) {
  const outdir = new URL("./dist", import.meta.url).pathname;
  try {
    for (const output of await buildApp({ outdir, production: true })) {
      const kib = (output.size / 1024).toFixed(1);
      console.log(`${output.path.slice(outdir.length + 1).padEnd(40)} ${kib.padStart(7)} KiB`);
    }
  } catch (error) {
    if (error instanceof AggregateError) for (const log of error.errors) console.error(log);
    console.error(error instanceof Error ? error.message : error);
    process.exit(1);
  }
}
