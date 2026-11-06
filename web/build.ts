// Production build: bundles index.html with its script and stylesheet into `dist/`, which
// board-server embeds and serves at `/` with an SPA fallback. Asset URLs are absolute
// (`/assets/...`) so they resolve from any route, such as `/tasks/factory-12`.

import { rm } from "node:fs/promises";

const outdir = new URL("./dist/", import.meta.url).pathname;
await rm(outdir, { recursive: true, force: true });

const result = await Bun.build({
  entrypoints: [new URL("./index.html", import.meta.url).pathname],
  outdir,
  target: "browser",
  minify: true,
  sourcemap: "none",
  publicPath: "/",
  naming: {
    entry: "[name].[ext]",
    chunk: "assets/[name]-[hash].[ext]",
    asset: "assets/[name]-[hash].[ext]",
  },
  define: { "process.env.NODE_ENV": JSON.stringify("production") },
});

if (!result.success) {
  for (const log of result.logs) console.error(log);
  process.exit(1);
}

// Defence in depth for agent-written text: only this origin's scripts, styles and API, no
// remote images. Added at build time because the dev server's hot reload injects scripts.
// The server can send the same policy as a header, which also allows `frame-ancestors`.
const CSP = [
  "default-src 'self'",
  "script-src 'self'",
  "style-src 'self'",
  "img-src 'self' data:",
  "connect-src 'self'",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'self'",
].join("; ");

const indexFile = Bun.file(`${outdir}index.html`);
const html = await indexFile.text();
const meta = `<meta http-equiv="Content-Security-Policy" content="${CSP}" />`;
if (!html.includes("<meta charset=\"utf-8\" />")) throw new Error("index.html lost its charset meta");
await Bun.write(indexFile, html.replace("<meta charset=\"utf-8\" />", `<meta charset="utf-8" />\n    ${meta}`));

for (const output of result.outputs) {
  const kib = (output.size / 1024).toFixed(1);
  console.log(`${output.path.slice(outdir.length).padEnd(40)} ${kib.padStart(7)} KiB`);
}
