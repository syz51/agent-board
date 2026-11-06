# agent-board

A shared, durable work board for coding-agent threads, and the orchestration that lets them
work off a backlog without someone watching.

Agent threads (Claude Code, Codex; inside [T3 Code](https://github.com/pingdotgg/t3code) or
standalone) on several machines read and write one board:

- each thread keeps its working state (its tasks and a handoff note) across context
  compaction and restarts;
- any thread, or you, can see where things stand without reading transcripts;
- threads get a short snapshot at session start, a delta each turn, and alerts when a peer
  edits files they changed or work they wait on finishes;
- agents add tasks as they find them, claim before working, and close tasks only with
  evidence;
- on boards switched to autopilot, a coordinator thread per project works off the ready
  queue and asks you only for decisions, which reach your phone.

Status: early development. The design is in [`docs/DESIGN.md`](docs/DESIGN.md) and the
HTTP contract in [`docs/API.md`](docs/API.md).

## How it fits together

One Rust binary, `board`:

| Command | Role |
|---|---|
| `board serve` | The server: SQLite, HTTP API, event stream, web view, Telegram notifier |
| `board mcp` | Stdio MCP server the agent runtime starts |
| `board hook <event>` | Hook handler for Claude Code and Codex |
| `board watch` | Device daemon: starts and wakes coordinator threads |
| `board ...` | CLI for you and for scripts |

The server is meant to run on one host on a private network (Tailscale); every device
talks to it with its own token.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## License

MIT
