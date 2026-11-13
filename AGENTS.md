# Working in agent-board

- Read `docs/DESIGN.md` and `docs/API.md` before changing behavior. `crates/board-proto` is
  the wire contract shared by every crate and the web view: change it, `docs/API.md` and
  their tests together, and keep changes backward compatible within `/v1`.
- This repository is public. Never commit server addresses, hostnames, tokens, Telegram ids,
  bucket names or board contents; examples use placeholders.
- Crate boundaries: `board-proto` (types, validation), `board-server` (storage, rules, API,
  rendering, notifier, web assets), `board` (binary: CLI, MCP, hooks, daemon, config). Add a
  crate's dependencies in its own manifest; only shared ones go in the workspace manifest.
- Validate input at the API boundary; inside, pass typed values. Errors returned to agents
  say what to do next.
- Checks before every commit: `cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`; for `web/`, its type check and
  build; for `deploy/`, `shellcheck deploy/*.sh`. Add tests with every behavior change.
- Time comes from an injectable clock so expiry and staleness are testable.
- The board never loses information (`docs/DESIGN.md`, "The trail rule"). Code that holds,
  refuses, stops, retries, gives back or drops work writes a record first: the time, the
  cause with its numbers, the thread or agent, and the whole of what was asked for. Put it
  on the board (a note, an event, an ask), or in a file under `$BOARD_HOME/state/` that a
  board record names; a log line alone is not a record. Add the record and its test in the
  same change.

## Merging

- `main` takes changes only through a PR whose `rust`, `web` and `docker` checks pass on a
  branch that is up to date with `main`, with every review thread resolved. Keep that rule.
- When a PR is ready, run `gh pr merge <N> --merge --auto`; GitHub merges it once the checks
  pass. When it is behind `main`, run `gh pr update-branch <N>` too, so GitHub merges `main`
  into it and reruns the checks, instead of merging `main` by hand and waiting on CI.
- Merge `main` locally only when `gh pr update-branch` reports a conflict; resolve it, run the
  checks above, and push.
