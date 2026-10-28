# zainod: static musl binary (jemalloc) on Alpine, uid 1000, no build args needed
#
# - docker run -p 8137:8137 -v ./zainod.toml:/etc/zaino/zainod.toml:ro -v zaino:/var/lib/zaino <image>
# - bases pinned by digest (frozen apk index = reproducible; dependabot bumps), apk unpinned

# Same toolchain as rust-toolchain.toml (else rustup downloads that one at build)
FROM docker.io/library/rust:1.96.0-alpine3.24@sha256:f87aa870663e2b57ec8c69de82c7eedf7383bee987eef7612c0359635eaadb41 AS builder
WORKDIR /src

# `release` or `profiling` (line tables + frame pointers)
ARG CARGO_PROFILE=release

# C toolchain (jemalloc, zstd, secp256k1) + protoc + its well-known types (google/protobuf/*.proto)
RUN apk add --no-cache build-base protoc protobuf-dev

COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    if [ "${CARGO_PROFILE}" = "profiling" ]; then export RUSTFLAGS="-C force-frame-pointers=yes"; fi; \
    cargo install --locked --path packages/zainod --bin zainod --root /out --profile "${CARGO_PROFILE}"

FROM docker.io/library/alpine:3.24@sha256:71e9b7bae160ba167fa17414401b94cbcdaf3c32feeab9298bd794a2a0614c35

# aria2c = the `[snapshot]` bootstrap's downloader
RUN apk add --no-cache ca-certificates aria2 && \
    addgroup -S -g 1000 zaino && \
    adduser -S -D -H -u 1000 -G zaino -h /var/lib/zaino zaino && \
    install -d -o zaino -g zaino /var/lib/zaino && \
    install -d /etc/zaino/tls

# - zainod's XDG defaults → /etc/zaino/zainod.toml + /var/lib/zaino/{indexes,state,peers}
# - gRPC on every interface (127.0.0.1 default = unreachable through a published port)
ENV XDG_CONFIG_HOME=/etc \
    XDG_CACHE_HOME=/var/lib \
    XDG_DATA_HOME=/var/lib \
    ZAINO_CONFIG_SERVE__GRPC_LISTEN_ADDRESS=0.0.0.0:8137

COPY --from=builder /out/bin/zainod /usr/local/bin/zainod

USER zaino
EXPOSE 8137

# Public /readyz: 200 = serving | degraded (wget: non-2xx = exit 1)
# - plaintext, else [serve.tls] (own loopback listener: certificate unchecked)
# - another port = override (`--health-cmd`, compose `healthcheck:`)
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --start-interval=5s \
    CMD wget -q -O /dev/null http://127.0.0.1:8137/readyz || \
        wget -q -O /dev/null --no-check-certificate https://127.0.0.1:8137/readyz

ENTRYPOINT ["zainod"]
CMD ["start"]
