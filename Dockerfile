# The board server image: the web view built with Bun, embedded into the `board` binary.
FROM oven/bun:1.4.4-slim AS web
WORKDIR /src/web
COPY web/package.json web/bun.lock ./
RUN bun install --frozen-lockfile
COPY web/ ./
RUN bun run build

FROM rust:1.99.1-slim-trixie AS build
# The commit the image is built from (no .git in the build context); /healthz reports it.
ARG BOARD_COMMIT=unknown
ENV BOARD_COMMIT=${BOARD_COMMIT}
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY prompts/ prompts/
COPY --from=web /src/web/dist web/dist
RUN cargo build --release --locked -p board

FROM debian:trixie-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
USER 10001:10001
ENTRYPOINT ["board"]
CMD ["serve", "--config", "/etc/agent-board/server.toml"]

# The published image (`--target release`): the static binaries from the release workflow,
# unpacked to dist/linux-<arch>/board, so the image runs the same binary as the archives.
FROM runtime AS release
ARG TARGETARCH
COPY dist/linux-${TARGETARCH}/board /usr/local/bin/board

# The default target: built from source, as `deploy/compose.yaml` does.
FROM runtime
COPY --from=build /src/target/release/board /usr/local/bin/board
