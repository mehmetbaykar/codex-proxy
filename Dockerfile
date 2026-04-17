FROM lukemathwalker/cargo-chef:latest-rust-1.91-bookworm AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --locked --release --bin codex-proxy

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --home-dir /app --shell /usr/sbin/nologin app

WORKDIR /app
COPY --from=builder /app/target/release/codex-proxy /usr/local/bin/codex-proxy

RUN mkdir -p /app/state/auth /app/state/db /app/state/files /app/state/logs \
    && chown -R app:app /app

ENV PORT=3000
ENV STATE_ROOT=/app/state
ENV RUST_LOG=info

USER app

EXPOSE 3000

ENTRYPOINT ["/usr/local/bin/codex-proxy"]
