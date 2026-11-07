# agent-board

[![CI](https://github.com/syz51/agent-board/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/syz51/agent-board/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

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

Status: early development. The design is in [`docs/DESIGN.md`](docs/DESIGN.md), the
HTTP contract in [`docs/API.md`](docs/API.md) and deployment in
[`docs/DEPLOY.md`](docs/DEPLOY.md).

## How it fits together

One Rust binary, `board`:

| Command | Role |
|---|---|
| `board serve` | The server: SQLite, HTTP API, event stream, web view, Telegram notifier |
| `board mcp` | Stdio MCP server the agent runtime starts |
| `board hook <event>` | Hook handler for Claude Code and Codex |
| `board watch` | Device daemon: starts (or adopts, after `board board adopt`) and wakes coordinator threads |
| `board ...` | CLI for you and for scripts |

The server is meant to run on one host on a private network (Tailscale); every device
talks to it with its own token.

## Install

Each [release](https://github.com/syz51/agent-board/releases) has `board` archives for Linux
(x86_64 and aarch64, statically linked) and macOS (Apple silicon and Intel), with the web
view built in, and a `SHA256SUMS` file:

```sh
shasum -a 256 --check --ignore-missing SHA256SUMS
gh attestation verify board-<version>-<target>.tar.gz --repo syz51/agent-board   # optional
tar -xzf board-<version>-<target>.tar.gz   # then put board on your PATH
```

The server also ships as a container image for `linux/amd64` and `linux/arm64`:

```sh
docker pull ghcr.io/syz51/agent-board:<version>
```

[`docs/DEPLOY.md`](docs/DEPLOY.md) covers running it, and `board install` sets up each device.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
(cd web && bun install && bun run check)   # web view: type check, tests, build
```

Build the web view before a release build of `board`, which embeds `web/dist`.

## Contributing

Issues and pull requests are welcome; [`CONTRIBUTING.md`](CONTRIBUTING.md) explains how.
Report vulnerabilities privately, as [`SECURITY.md`](SECURITY.md) describes.

## License

[MIT](LICENSE)
