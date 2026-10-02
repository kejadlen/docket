FROM ramekin-agent

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# The minimal profile omits these; `just` runs all three (llvm-tools for
# coverage).
RUN rustup component add rustfmt clippy llvm-tools

# bin/coverage reads profiles with grcov; same version as CI, built for
# this machine's architecture.
RUN curl --proto '=https' --tlsv1.2 -sSfL \
        "https://github.com/marco-c/grcov/releases/download/v0.10.7/grcov-$(uname -m)-unknown-linux-gnu.tar.bz2" \
    | tar xj -C /usr/local/bin
