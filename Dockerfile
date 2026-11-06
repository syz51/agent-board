# The board server image: the web view built with Bun, embedded into the `board` binary.
FROM oven/bun:1.4.4-slim AS web
WORKDIR /src/web
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile
COPY web/ ./
RUN bun run build

FROM rust:1.99.1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY prompts/ prompts/
COPY --from=web /src/web/dist web/dist
RUN cargo build --release --locked -p board

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/board /usr/local/bin/board
USER 10001:10001
ENTRYPOINT ["board"]
CMD ["serve", "--config", "/etc/agent-board/server.toml"]
