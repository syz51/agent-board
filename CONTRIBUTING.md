# Contributing to agent-board

Thanks for helping. The project is in early development, so the design still moves; open an
issue before you start on anything larger than a fix, so we can agree on the approach first.

## Set up

You need:

- Rust 1.99 or later (`rustup` installs the stable toolchain the workspace builds with);
- [Bun](https://bun.sh) at the version in `packageManager` in
  [`web/package.json`](web/package.json), for the web view.

```sh
git clone https://github.com/syz51/agent-board
cd agent-board
(cd web && bun install)
```

`cargo run -p board -- --help` lists the commands. For the web view, `bun run dev` in `web/`
serves it with hot reload against fixtures; [`web/README.md`](web/README.md) has the rest.
A release build of `board` embeds `web/dist`, so run `bun run build` there first.

## Before you push

Run the checks in the README's [Development](README.md#development) section. CI runs the
same ones, plus a Docker build, on every pull request.

## Conventions

[`AGENTS.md`](AGENTS.md) holds the project rules, for people and coding agents alike. The
ones that matter most in review:

- read [`docs/DESIGN.md`](docs/DESIGN.md) and [`docs/API.md`](docs/API.md) before changing
  behavior, and change `crates/board-proto`, `docs/API.md` and their tests together;
- add tests with every behavior change;
- this repository is public: examples, tests and fixtures use placeholders, never real
  hostnames, tokens, Telegram ids, bucket names or board contents.

## Proposing a change

1. For a bug or feature, find or open an [issue](https://github.com/syz51/agent-board/issues/new/choose).
   Report security problems privately instead, as [`SECURITY.md`](SECURITY.md) explains.
2. Fork, branch from `main`, and keep each pull request to one change.
3. Open the pull request and fill in the template. CI must pass before it merges;
   review conversations must be resolved.

By contributing you agree that your work is released under the [MIT licence](LICENSE), and
to follow the [code of conduct](CODE_OF_CONDUCT.md).
