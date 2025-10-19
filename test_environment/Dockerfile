# syntax=docker/dockerfile:1.4
# Begin storing hash-object for this file

########################
# === GLOBAL ARGS ===
########################
ARG RUST_VERSION
ARG ZCASH_VERSION
ARG ZEBRA_VERSION
ARG UID=1000
ARG GID=${UID}
ARG USER=container_user
ARG HOME="/home/container_user"
ARG CARGO_HOME="${HOME}/.cargo"
ARG CARGO_TARGET_DIR="${HOME}/target"

########################
# === DOWNLOADERS ===
########################

FROM zfnd/zebra:${ZEBRA_VERSION} AS zebra-downloader
FROM electriccoinco/zcashd:v${ZCASH_VERSION} AS zcashd-downloader

########################
# === BUILDERS ===
########################

FROM rust:${RUST_VERSION}-bookworm AS zebra-builder

ARG ZEBRA_VERSION
RUN apt-get update
RUN apt-get install -y git clang cmake pkg-config libssl-dev protobuf-compiler
WORKDIR /zebra
RUN git clone https://github.com/ZcashFoundation/zebra .
RUN git checkout "${ZEBRA_VERSION}" 
# Use persistent cache for cargo
# RUN --mount=type=cache,target=/usr/local/cargo/registry \
#     --mount=type=cache,target=/usr/local/cargo/git \
#     --mount=type=cache,target=/zebra/target \
RUN cargo build --release --bin zebrad
RUN cp target/release/zebrad /tmp/zebrad

FROM debian:bookworm AS zcashd-builder

ARG ZCASH_VERSION
RUN apt-get update && apt-get install -y \
    git build-essential pkg-config libssl-dev automake autoconf \
    libtool bsdmainutils curl

WORKDIR /zcash
RUN git clone --depth 1 --branch "v${ZCASH_VERSION}" https://github.com/zcash/zcash .
RUN ./zcutil/fetch-params.sh && ./zcutil/build.sh -j$(nproc)
RUN cp src/zcashd src/zcash-cli /tmp/

########################
# === BASE RUNTIME IMAGE ===
########################

FROM rust:${RUST_VERSION}-bookworm AS base

ARG UID
ARG GID
ARG USER
ARG HOME
ARG CARGO_HOME
ARG CARGO_TARGET_DIR

ENV CARGO_HOME=${CARGO_HOME}
ENV CARGO_TARGET_DIR=${CARGO_TARGET_DIR}
ENV CARGO_TERM_COLOR=always
ENV PROTOC=/usr/bin/protoc


LABEL maintainer="nachog00"
LABEL org.zaino.test-artifacts.versions.rust="${RUST_VERSION}"

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl-dev \
    curl \
    libclang-dev \
    build-essential \
    cmake \
    libsnappy-dev \
    zlib1g-dev \
    libbz2-dev \
    liblz4-dev \
    libzstd-dev \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Create container_user
RUN groupadd --gid ${GID} ${USER} && \
    useradd --uid ${UID} --gid ${GID} --home-dir ${HOME} --create-home ${USER}

# Install rustup (Rust toolchain manager) and set up the Rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
RUN rustup update stable

# Create artifacts directory
RUN mkdir -p /home/container_user/artifacts

# Copy and prepare entrypoint script
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

WORKDIR ${HOME}/zaino
RUN chown -R ${UID}:${GID} ${HOME}
USER ${USER}

########################
# === FINAL TARGETS ===
########################

FROM base AS final-prebuilt
ARG ZCASH_VERSION
ARG ZEBRA_VERSION
LABEL org.zaino.test-artifacts.versions.zcashd="${ZCASH_VERSION}"
LABEL org.zaino.test-artifacts.versions.zebra="${ZEBRA_VERSION}"
USER root
COPY --from=zcashd-downloader /usr/bin/zcashd ${HOME}/artifacts/
COPY --from=zcashd-downloader /usr/bin/zcash-cli ${HOME}/artifacts/
COPY --from=zebra-downloader /usr/local/bin/zebrad ${HOME}/artifacts/
RUN chmod +x ${HOME}/artifacts/zcashd ${HOME}/artifacts/zcash-cli ${HOME}/artifacts/zebrad
RUN cargo install cargo-nextest --locked --verbose --root /usr/local && \
    cargo install rust-script --locked --verbose --root /usr/local

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/bin/bash"]

FROM base AS final-zcashd-source
ARG ZCASH_VERSION
ARG ZEBRA_VERSION
LABEL org.zaino.test-artifacts.versions.zcashd="${ZCASH_VERSION}"
LABEL org.zaino.test-artifacts.versions.zebra="${ZEBRA_VERSION}"
USER root
COPY --from=zcashd-builder /tmp/zcashd ${HOME}/artifacts/
COPY --from=zcashd-builder /tmp/zcash-cli ${HOME}/artifacts/
COPY --from=zebra-downloader /usr/local/bin/zebrad ${HOME}/artifacts/
RUN chmod +x ${HOME}/artifacts/zcashd ${HOME}/artifacts/zcash-cli ${HOME}/artifacts/zebrad
RUN cargo install cargo-nextest --locked --verbose --root /usr/local && \
    cargo install rust-script --locked --verbose --root /usr/local

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/bin/bash"]

FROM base AS final-zebrad-source
ARG ZCASH_VERSION
ARG ZEBRA_VERSION
LABEL org.zaino.test-artifacts.versions.zcashd="${ZCASH_VERSION}"
LABEL org.zaino.test-artifacts.versions.zebra="${ZEBRA_VERSION}"
USER root
COPY --from=zcashd-downloader /usr/bin/zcashd ${HOME}/artifacts/
COPY --from=zcashd-downloader /usr/bin/zcash-cli ${HOME}/artifacts/
COPY --from=zebra-builder /tmp/zebrad ${HOME}/artifacts/
RUN chmod +x ${HOME}/artifacts/zcashd ${HOME}/artifacts/zcash-cli ${HOME}/artifacts/zebrad
RUN cargo install cargo-nextest --locked --verbose --root /usr/local && \
    cargo install rust-script --locked --verbose --root /usr/local

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/bin/bash"]

FROM base AS final-all-source
ARG ZCASH_VERSION
ARG ZEBRA_VERSION
LABEL org.zaino.test-artifacts.versions.zcashd="${ZCASH_VERSION}"
LABEL org.zaino.test-artifacts.versions.zebra="${ZEBRA_VERSION}"
USER root
COPY --from=zcashd-builder /tmp/zcashd ${HOME}/artifacts/
COPY --from=zcashd-builder /tmp/zcash-cli ${HOME}/artifacts/
COPY --from=zebra-builder /tmp/zebrad ${HOME}/artifacts/
RUN chmod +x ${HOME}/artifacts/zcashd ${HOME}/artifacts/zcash-cli ${HOME}/artifacts/zebrad
RUN cargo install cargo-nextest --locked --verbose --root /usr/local && \
    cargo install rust-script --locked --verbose --root /usr/local

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/bin/bash"]
