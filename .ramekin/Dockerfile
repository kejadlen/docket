FROM ramekin-agent

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# The minimal profile omits these; `just` runs both.
RUN rustup component add rustfmt clippy
