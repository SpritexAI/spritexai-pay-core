# SpritEXAI Pay — core engine image.
# Author: Mohammad Sijan (SpritexAI).
#
# Multi-stage: a fat builder compiles the release binary, then we copy just the
# static-ish binary into a minimal Debian-slim runtime. Rust builds are slow, so
# CI does the compiling (see .github/workflows) and the VPS only ever pulls this
# finished image — it never compiles.

FROM rust:1.90-slim-bookworm AS builder
WORKDIR /build

# Cache dependencies separately from source for faster incremental CI builds.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release --locked 2>/dev/null || true
RUN rm -rf src

COPY . .
# Touch so cargo rebuilds against the real sources after the dependency cache layer.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:bookworm-slim AS runtime
# ca-certificates is needed for outbound HTTPS webhook delivery (rustls trust store).
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 spritex

WORKDIR /app
COPY --from=builder /build/target/release/spritexai-pay /usr/local/bin/spritexai-pay

# Data dir for the default SQLite database; override DATABASE_URL for Postgres.
RUN mkdir -p /data && chown spritex:spritex /data
USER spritex
ENV DATABASE_URL="sqlite:///data/spritexai_pay.db?mode=rwc" PORT=8080
EXPOSE 8080
VOLUME ["/data"]

ENTRYPOINT ["/usr/local/bin/spritexai-pay"]
