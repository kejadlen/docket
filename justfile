default: all

fmt:
    cargo fmt --all

check:
    cargo check --workspace --all-features

clippy:
    cargo clippy --workspace --all-features -- -D warnings

coverage:
    ./bin/coverage

mutants:
    #!/usr/bin/env bash
    set -uo pipefail
    cargo mutants --timeout-multiplier 3 -j4
    rc=$?
    # 0 = all caught, 3 = timeouts (infinite loops from mutants, still caught).
    if [ "$rc" -eq 0 ] || [ "$rc" -eq 3 ]; then
        exit 0
    fi
    exit "$rc"

all: fmt clippy coverage

# Serve the fixture-backed UI without Tailscale on http://127.0.0.1:3000,
# restarting on changes.
dev:
    { fd -t f . src assets; echo Cargo.toml docket.kdl; } | entr -r cargo run --features dev

install:
    cargo install --locked --path .
