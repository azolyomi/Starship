# syntax=docker/dockerfile:1.7
#
# Multi-stage build: Rust 1.95 builder -> Debian slim runtime.
# Final image only carries the statically-linked-ish `starship` binary, the
# migrations directory (read at runtime via `sqlx::migrate!`), the
# data/ files (seeded into Postgres via `templates::load_and_seed`), and
# `ca-certificates` (HTTPS to Discord + RealmEye).
#
# No native libs: every TLS path uses rustls, and songbird/opus were
# removed in Phase 6.
#
# Build:   docker build -t starship:latest .
# Run:     docker compose up -d    (see docker-compose.yml)

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1.95.0-slim-bookworm AS builder

WORKDIR /app

# Warm the dependency cache first: copy only the manifests, build a stub
# crate, then swap in the real sources. Subsequent rebuilds that only touch
# src/ skip re-downloading and re-compiling deps.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/starship target/release/deps/starship-*

# Real sources. `touch` forces cargo to rebuild the workspace crate.
COPY src ./src
COPY migrations ./migrations
COPY data ./data
RUN touch src/main.rs \
    && cargo build --release --locked

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates: HTTPS to discord.com + realmeye.com
# tini: clean PID 1 + proper signal forwarding (so `docker compose down`
#       issues SIGTERM and tokio can shut down gracefully)
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Non-root user. Dropping root is cheap here — the bot writes nothing to
# disk at runtime (all state lives in Postgres).
RUN useradd --system --home-dir /app --shell /usr/sbin/nologin starship \
    && chown -R starship:starship /app
USER starship

COPY --from=builder --chown=starship:starship /app/target/release/starship /usr/local/bin/starship
COPY --from=builder --chown=starship:starship /app/migrations ./migrations
COPY --from=builder --chown=starship:starship /app/data ./data

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/starship"]
CMD ["bot"]
