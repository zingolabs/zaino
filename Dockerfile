# Specify the base image for building
FROM rust:1.86.0-bookworm AS builder

# Accept an argument to control no-tls builds
ARG NO_TLS=false

RUN apt-get update && apt-get install -y --no-install-recommends \
    musl-dev \
    gcc \
    clang \
    llvm-dev \
    libclang-dev \
    cmake \
    make \
    # Install OpenSSL only if not building with no-tls
    && if [ "$NO_TLS" = "false" ]; then apt-get install -y libssl-dev; fi \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Dependency caching: create a dummy project structure
RUN cargo new --bin zainod && \
    cargo new --bin integration-tests && \
    cargo new --lib zaino-fetch && \
    cargo new --lib zaino-proto && \
    cargo new --lib zaino-serve && \
    cargo new --lib zaino-state && \
    cargo new --lib zaino-testutils && \
    echo "pub fn main() {}" > zainod/src/lib.rs && \
    echo "pub fn main() {}" > integration-tests/src/lib.rs

COPY Cargo.toml Cargo.lock ./
COPY integration-tests/Cargo.toml integration-tests/
COPY zaino-fetch/Cargo.toml zaino-fetch/
COPY zaino-proto/Cargo.toml zaino-proto/
COPY zaino-serve/Cargo.toml zaino-serve/
COPY zaino-state/Cargo.toml zaino-state/
COPY zaino-testutils/Cargo.toml zaino-testutils/
COPY zainod/Cargo.toml zainod/

# Build dependencies only for the given features
RUN cargo fetch && \
    if [ "$NO_TLS" = "true" ]; then \
        echo "Building with no TLS"; \
        cargo build --release --all-targets --workspace --features disable_tls_unencrypted_traffic_mode; \
    else \
        echo "Building with TLS"; \
        cargo build --release --all-targets --workspace; \
    fi && \
    # Remove the dummy source files but keep the target directory
    rm -rf */src

# Now build everything
COPY . .

# Build the final application according to the NO_TLS flag
RUN find . -name "*.rs" -exec touch {} + && \
    if [ "$NO_TLS" = "true" ]; then \
        cargo build --release --features disable_tls_unencrypted_traffic_mode; \
    else \
        cargo build --release; \
    fi

# Final stage using a slim base image
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssl \
    libc6 \
    libgcc-s1 && \
    rm -rf /var/lib/apt/lists/*

RUN groupadd -r -g 2003 zaino && useradd -r -u 2003 -g zaino zaino

WORKDIR /app

COPY --from=builder /app/target/release/zainod /app/zainod

RUN chown zaino:zaino /app/zainod

USER zaino

EXPOSE 8137

ENTRYPOINT ["/app/zainod"]
