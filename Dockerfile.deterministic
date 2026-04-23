# syntax=docker/dockerfile:1

############################
# Global build args
############################
ARG RUST_VERSION=1.94.0
ARG UID=1000
ARG GID=1000
ARG USER=container_user
ARG HOME=/home/container_user

############################
# Dependencies 
############################
FROM stagex/pallet-rust:1.94.0@sha256:2fbe7b164dd92edb9c1096152f6d27592d8a69b1b8eb2fc907b5fadea7d11668 AS pallet-rust
FROM stagex/pallet-clang@sha256:07c01477a41eba3ec57a0e84c73659dec17662247a8f92b8b902f0aa02b58ca3 AS pallet-clang
FROM stagex/user-protobuf:26.1@sha256:a135aaf060990b6ef8a7c715c16f175811d3a1f5383970f5771adef05a0bc56a AS protobuf
FROM stagex/user-abseil-cpp:20240116.2@sha256:20a241145158a0aa7cb83ed5dc4f9ad6360dc975352787f4e6b00e8a39943f62 AS abseil-cpp
FROM stagex/core-busybox:1.37.0@sha256:d608daa946e4799cf28b105aba461db00187657bd55ea7c2935ff11dac237e27 AS busybox

############################
# Builder
############################
FROM pallet-rust AS builder

SHELL ["/bin/sh", "-euo", "pipefail", "-c"]
COPY --from=pallet-clang . /
COPY --from=protobuf . /
COPY --from=abseil-cpp . /

WORKDIR /usr/src/app

# Toggle to build without TLS feature if needed
ARG NO_TLS=false

ENV CARGO_HOME=/usr/local/cargo

ENV CXXSTDLIB="c++"

ENV RUST_BACKTRACE=1
ENV RUSTFLAGS="-C codegen-units=1"
ENV RUSTFLAGS="${RUSTFLAGS} -C target-feature=+crt-static"
ENV RUSTFLAGS="${RUSTFLAGS} -C linker=clang -C link-arg=-fuse-ld=mold"
ENV RUSTFLAGS="${RUSTFLAGS} -C link-arg=/usr/lib/libc++.a"
ENV RUSTFLAGS="${RUSTFLAGS} -C link-arg=/usr/lib/libc++abi.a"
ENV RUSTFLAGS="${RUSTFLAGS} -C link-arg=-Wl,--build-id=none"
ENV SOURCE_DATE_EPOCH=1
ENV CXXFLAGS="-stdlib=libc++ -include cstdint"
ENV ROCKSDB_USE_PKG_CONFIG=0

ENV TARGET_ARCH="x86_64-unknown-linux-musl"

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo fetch --locked --target $TARGET_ARCH

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo metadata --locked --format-version=1 > /dev/null 2>&1

# Efficient caches + install to a known prefix (/out)
# This avoids relying on target/release/<bin> paths.
RUN --network=none \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/usr/src/app/target \
    if [ "${NO_TLS}" = "true" ]; then \
      cargo build --release --frozen --target $TARGET_ARCH --bin zainod --features no_tls_use_unencrypted_traffic; \
    else \
      cargo build --release --frozen --target $TARGET_ARCH --bin zainod; \
    fi && \
    install -D -m 0755 /usr/src/app/target/${TARGET_ARCH}/release/zainod /usr/local/bin/zainod

############################
# Export stage 
############################
FROM scratch AS export
COPY --from=builder /usr/local/bin/zainod /zainod

############################
# Runtime (slim, non-root)
############################
FROM busybox AS runtime

ARG HOME

WORKDIR ${HOME}

# Copy the installed binary from builder
COPY --from=export /zainod /

# Default ports (adjust if your app uses different ones)
ARG ZAINO_GRPC_PORT=8137
ARG ZAINO_JSON_RPC_PORT=8237
EXPOSE ${ZAINO_GRPC_PORT} ${ZAINO_JSON_RPC_PORT}

# Healthcheck that doesn't assume specific HTTP/gRPC endpoints
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD /usr/local/bin/zainod --version >/dev/null 2>&1 || exit 1

ENTRYPOINT ["/zainod"]
CMD []
