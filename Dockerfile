FROM rust:1.86.0-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    musl-dev \
    gcc \
    clang \
    llvm-dev \
    libclang-dev \
    libssl-dev \
    cmake \
    make && \
    rm -rf /var/lib/apt/lists/*

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

# Build dependencies only
RUN cargo fetch && \
    cargo build --release --all-targets --workspace && \
    # Remove the dummy source files but keep the target directory
    rm -rf */src

# Now build everything
COPY . .

RUN find . -name "*.rs" -exec touch {} + && \
    cargo build --release

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
