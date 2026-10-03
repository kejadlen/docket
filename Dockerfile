# Cargo-chef stage for dependency caching.
FROM rust:1.95-trixie AS chef
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    cargo install --locked cargo-chef
WORKDIR /build

# Plan stage: inspect source and produce a recipe of dependencies.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Build stage: cook dependencies first (cached across rebuilds), then build
# the binary. Changes to source code do not retrigger dependency compilation.
FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/build/target \
    cargo chef cook --release --recipe-path recipe.json
COPY . .
# Cargo features to build with. Empty for the release image; the Fly demos
# set `dev` (see fly.toml).
ARG FEATURES=""
# Copy the binary out of the cache mount so it survives into the runtime
# stage.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin docket --features "$FEATURES" && \
    cp target/release/docket /build/docket

# Runtime stage.
FROM debian:trixie-slim

# JMAP over TLS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/docket /usr/local/bin/docket
COPY fly.kdl /etc/docket.kdl

EXPOSE 3000
ENTRYPOINT ["docket"]
