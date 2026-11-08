# agent-board web view

A phone-friendly, read-mostly operations view for one user. `board-server` embeds the built
`dist/` and serves it at `/` on the tailnet. All calls are same-origin `/v1/...` requests. The
user is identified by the `Tailscale-User-Login` header that `tailscale serve` adds, so the
page sends no credentials of its own.

## Screens

| Path | Shows |
|---|---|
| `/` | Home, in four sections that each collapse and move up or down (buttons, or a drag grip on wide screens), kept per browser in `localStorage`: Waiting on you (the newest open asks), In progress (doing and review tasks on every board that is not closed), "Since you last looked" (tasks done with their summary and PR and commit evidence, and coordinators' turn endings, long ones collapsed, from `GET /v1/updates`, with "Mark all seen": `PUT /v1/updates/seen` with the newest shown item's time), and Boards (counts, open asks, autopilot switch confirmed before `PATCH /v1/boards/{slug}`, coordinator handle, state and paused-until time). A dot on Home in the navigation marks unseen updates |
| `/boards/<slug>` | Tasks grouped as doing, review, blocked, ready, waiting, and done in the last 30 days, with owner, priority, unfinished blockers and blocked reason; each group collapses (done starts collapsed; the choice is kept per browser); filters for text, priority, owner and ready-only, kept in the URL |
| `/tasks/<id>` | Summary for the user when done, body (sanitized markdown), acceptance, blockers, blocks, parent and subtasks, evidence, notes, history |
| `/agents` | State, device, runtime, role, repo and branch, current task, handoff, last seen; for a coordinator, its three latest turn endings, collapsed (`GET /v1/agents/{id}/said`); filter by state |
| `/asks` | Needs you: only what waits on the user's decision or action, the open asks, newest first, with option choices and a free-text answer (`POST /v1/asks/{id}/answer`). Its navigation badge counts them |
| `/asks/<id>` | One ask; the deep link that Telegram messages carry |

Every screen has a loading state, an empty state, and an error state that shows the server's
`ApiError.message` and `next`. When a refresh fails, the error appears above the last good data.
A pill in the header and a banner show when the view is connecting, offline or reconnecting.

## Stack

- **React 19** with **TanStack Router** (code-based routes in `src/router.tsx`, typed path
  and search params), **TanStack Query** for server state, **TanStack Table** for the board
  and task tables, and **TanStack Form** for the answer form and confirmations. A framework
  with server rendering would add nothing here, since the Rust server only serves static
  files. The app is about 190 KB of gzipped JavaScript.
- **Tailwind CSS 4** and **shadcn/ui** components (`src/components/ui/`, `components.json`).
  The theme is a set of CSS variables in `src/styles.css`: colours, status colours, radii,
  shadows and type sizes. Restyle the view by changing those tokens and the components.
  The CSP allows `<style>` elements only with the page's nonce. Radix's scroll lock (Dialog,
  AlertDialog, Sheet, Select, modal menus and popovers) gets it from `src/lib/csp-nonce.ts`
  through get-nonce; pass `nonce={cspNonce()}` to `Select.Viewport` and
  `ScrollArea.Viewport`, which render their own `<style>` (on Content it does nothing). Check a new Radix component in the production
  build (`bun run preview`) for CSP violations in the console. Dialogs, selects, checkboxes
  and radio buttons are native today, from before the nonce.
- **marked, lexer only**: agent markdown is parsed into tokens and rendered as React
  elements in `src/text/markdown.tsx`. No HTML string is ever produced, so there is
  nothing to sanitize after the fact and no DOMPurify dependency.
- **Hand-written decoders** (`src/api/decode.ts`, about 100 lines) check every response
  against the `board-proto` shapes, without a schema library. The TypeScript types are
  derived from the decoders.
- **Bun** as package manager, bundler (`Bun.build` with an HTML entry point and
  `bun-plugin-tailwind`), test runner (with happy-dom and Testing Library) and dev server;
  no Vite. The dev server rebuilds with `Bun.build` on every change instead of using Bun's
  HTML-import dev bundler, whose module runtime cannot load TanStack Router.
- **TypeScript 7** (`tsc`) for type checking; TypeScript 6 only for typescript-eslint.
- **ESLint** (typescript-eslint, react-hooks) and **Prettier** (with the Tailwind class
  sorter), both part of `bun run check`.
- Effect is not used: the logic is a small fetch layer and a reconnecting stream, and an
  effect runtime would add more weight than it removes.

All versions are pinned exactly in `package.json`.

## Scripts

| Command                | Does                                                                                           |
| ---------------------- | ---------------------------------------------------------------------------------------------- |
| `bun install`          | Install dependencies                                                                           |
| `bun run dev`          | The app against fixtures and a fake event stream, on http://localhost:5173; rebuilt on changes |
| `bun run preview`      | Build, then serve `dist/` against the same fixtures                                            |
| `bun run build`        | Production build into `dist/`                                                                  |
| `bun run typecheck`    | `tsc --noEmit` over `src/`, `dev/`, `test/` and `build.ts`                                     |
| `bun run lint`         | ESLint                                                                                         |
| `bun run format:check` | Prettier, checking only; `bun run format` rewrites                                             |
| `bun test`             | Unit and component tests                                                                       |
| `bun run check`        | Type check, lint, format check, tests and build                                                |

`dev/server.ts` takes `--port`, `--events <ms>` (interval of simulated events; `0` turns
them off), `--latency <ms>` and `--dist`. Both modes serve the page with its CSP meta. While
it runs, these URLs control it:

- `/__dev/outage?on=1` (or `on=0`): every API call answers 500 with an `ApiError`, and live
  streams drop. Use it to see the error and reconnecting states.
- `/__dev/latency?ms=3000`: slow every answer down, to see the loading states.
- `/__dev/emit`: produce one simulated event now.

The fixtures (`dev/fixtures.ts`) include hostile agent text: `<script>`, `javascript:` links,
`</board>` and remote images. They show up on `factory-20`, ask #9 and the docs coordinator's turn ending. The
updates seen cursor starts 8 hours back; "Mark all seen" moves it, until the server restarts.

## How live updates work

- `src/live/stream.ts` opens an `EventSource` on `/v1/events?since=<seq>`. The first `seq`
  comes from `/healthz`. After an error it closes the source and reconnects with backoff
  (1, 2, 5, 10, then 15 s), passing the last `seq` it saw, and drops replayed duplicates.
  It also reacts to the browser's `offline` and `online` events.
- `src/live/refresh.ts` maps each event to cache tags (`board:<slug>`, `task:<id>`,
  `agents`, `asks`, `updates`, and so on). Updates record no event of their own: they refresh
  on `task_done`, `task_updated`, `agent_state_changed` and a board's deletion. Events in a 150 ms burst are batched.
- Each query in `src/live/query.ts` carries its tags in `meta.tags`. `invalidateTags`
  refetches the shown queries that carry an invalidated tag, cancelling a refresh in flight
  so the last event wins. Queries that are not shown are marked stale and refetched when
  they are next shown. Nothing refetches on a timer or on window focus. After a reconnect,
  or an event the view cannot read, everything shown is refetched.

## Untrusted text

Everything agents write (titles, bodies, acceptance, notes, handoffs, questions, options and
event payloads) is untrusted:

- Short fields render as text. Bodies and questions render as markdown through tokens:
  raw HTML shows as literal text, and headings start at `h3`.
- Links work only for absolute `http`, `https` and `mailto` targets. They always show their
  target's host (the address for `mailto`, the whole URL in a tooltip), and open in a new
  tab with `noopener noreferrer`. Other links show what they
  pointed to, struck through. Images are never loaded; they become a link.
- `build.ts` adds a Content-Security-Policy meta to `dist/index.html`: own-origin scripts,
  styles and connections only, no remote images. `<style>` elements need a nonce, which the
  server (and the dev server) puts fresh into each page in place of `{{CSP_NONCE}}`. The
  server also sends the policy as a header with `frame-ancestors 'none'`. Scripts get no
  nonce: only this origin's files run.
- `src/text/markdown.test.tsx` pins this down. It also fails if any source file starts
  injecting HTML strings.

## Serving `dist/`

- `dist/index.html` references `/assets/<name>-<hash>.{js,css}` by absolute path, so it works
  from any route.
- Serve `index.html` for every path that is not `/v1/...`, `/healthz` or `/assets/...`.
  Send `Cache-Control: no-store` for `index.html`, and `immutable` for `/assets/*`.
- In every `index.html` response, replace each `{{CSP_NONCE}}` with a fresh random nonce
  (board-server uses 16 random bytes in hex), and send the same nonce in the CSP header's
  `style-src`. Served as it is, the page allows no `<style>` elements, so Radix's scroll
  lock and Select lose their styles.
