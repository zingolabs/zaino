# syntax=docker/dockerfile:1

############################
# Global build args
############################
ARG RUST_VERSION=1.86.0
ARG UID=1000
ARG GID=1000
ARG USER=container_user
ARG HOME=/home/container_user

############################
# Dependencies 
############################
FROM stagex/core-busybox@sha256:637b1e0d9866807fac94c22d6dc4b2e1f45c8a5ca1113c88172e0324a30c7283 AS busybox
FROM stagex/pallet-rust@sha256:9c38bf1066dd9ad1b6a6b584974dd798c2bf798985bf82e58024fbe0515592ca AS pallet-rust
FROM stagex/user-protobuf@sha256:5e67b3d3a7e7e9db9aa8ab516ffa13e54acde5f0b3d4e8638f79880ab16da72c AS protobuf 
FROM stagex/user-abseil-cpp@sha256:3dca99adfda0cb631bd3a948a99c2d5f89fab517bda034ce417f222721115aa2 AS abseil-cpp
FROM stagex/core-gcc@sha256:964ffd3793c5a38ca581e9faefd19918c259f1611c4cbf5dc8be612e3a8b72f5 AS gcc 
FROM stagex/core-musl@sha256:d9af23284cca2e1002cd53159ada469dfe6d6791814e72d6163c7de18d4ae701 AS musl
FROM stagex/core-libunwind@sha256:eb66122d8fc543f5e2f335bb1616f8c3a471604383e2c0a9df4a8e278505d3bc AS libunwind 
FROM stagex/core-user-runtime@sha256:055ae534e1e01259449fb4e0226f035a7474674c7371a136298e8bdac65d90bb AS user-runtime

############################
# Builder
############################
FROM pallet-rust AS builder
COPY --from=stagex/bash . /
COPY --from=protobuf . /
COPY --from=abseil-cpp . /
COPY --from=busybox . /

SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
WORKDIR /usr/src/app

# Toggle to build without TLS feature if needed
ARG NO_TLS=false

# Copy entire workspace (prevents missing members)
ENV SOURCE_DATE_EPOCH=1
ENV CXXFLAGS="-include cstdint"
ENV ROCKSDB_USE_PKG_CONFIG=0
ENV CARGO_HOME=/usr/local/cargo

ENV RUST_BACKTRACE=1
ENV RUSTFLAGS="-C codegen-units=1"
ENV RUSTFLAGS="${RUSTFLAGS} -C target-feature=+crt-static"
ENV RUSTFLAGS="${RUSTFLAGS} -C link-arg=-Wl,--build-id=none"
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
FROM user-runtime AS runtime
COPY --from=busybox . /
COPY --from=gcc  /usr/lib/libgcc_s.so.1 /usr/lib/
COPY --from=gcc  /usr/lib/libstdc++.so.6 /usr/lib/
COPY --from=musl /lib/ld-musl-x86_64.so.1 /lib/
COPY --from=libunwind /lib/libunwind.so.8 /lib/

ARG HOME

WORKDIR ${HOME}

# Copy the installed binary from builder
COPY --from=builder /usr/local/bin/zainod /usr/local/bin/zainod

# Default ports (adjust if your app uses different ones)
ARG ZAINO_GRPC_PORT=8137
ARG ZAINO_JSON_RPC_PORT=8237
EXPOSE ${ZAINO_GRPC_PORT} ${ZAINO_JSON_RPC_PORT}

# Healthcheck that doesn't assume specific HTTP/gRPC endpoints
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD /usr/local/bin/zainod --version >/dev/null 2>&1 || exit 1

ENTRYPOINT ["/usr/local/bin/zainod"]
CMD []
