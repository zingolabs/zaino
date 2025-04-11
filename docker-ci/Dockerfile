# This Dockerfile creates an image for running cargo nextest on a Rust repository.
# Use the following command to run it:
# ```
# docker run --rm -v /path/to/your/repo:/app my-nextest-image [nextest-options]
# ```

# # Stage 1: Common Dependencies
# FROM ubuntu:20.04 AS common-deps

# RUN apt update && apt-get install -y --no-install-recommends \
#     build-essential \
#     coreutils \
#     pkg-config \
#     libssl-dev \
#     git \
#     curl \
#     && rm -rf /var/lib/apt/lists/*

# Stage 2: Build lightwalletd
FROM golang:1.19 AS lightwalletd-builder

WORKDIR /usr/src

# Clone and build lightwalletd
RUN git clone --branch v0.4.16 --depth 1 https://github.com/zcash/lightwalletd && \
    cd lightwalletd && \
    make && \
    cp lightwalletd /usr/local/bin/

# Stage 3: Build zcashd and zcash-cli
FROM rust:latest AS zcash-builder

WORKDIR /usr/src

# Install needed compiling tooling
RUN apt update && apt-get install -y --no-install-recommends build-essential coreutils bsdmainutils

# Clone and build zcash
RUN git clone --branch v6.1.0 --depth 1 https://github.com/zcash/zcash.git && \
    cd zcash/ && \
    # ./zcutil/fetch-params.sh && \
    # ./zcutil/clean.sh && \
    ./zcutil/build.sh -j$(nproc) && \
    cp src/zcashd /usr/local/bin/ && \
    cp src/zcash-cli /usr/local/bin/

# Stage 4: Build zebrad
# Using a release zebrad iamge for now
FROM zfnd/zebra:latest AS zebrad-release
  
# Stage 5: Final Image
FROM rust:latest

LABEL maintainer="nachog00" \
    version="1.0" \
    description="This image provides several testing zcash-related executables in $PATH. Use this image in CI workflows for testing, or as a base for running local integration tests." \
    usage="For CI, run with the container option defined in your workflow YAML and use the checkout action. For local testing, you can run: docker run --rm -v /path/to/zaino/repo:/app zaino-ci [nextest-options]" \
    zcashd.version="v6.1.0" \
    lightwalletd.version="v0.4.16"

# Install Nextest
RUN cargo install cargo-nextest --locked --verbose

# Install
RUN apt update && apt-get install -y --no-install-recommends \
    libclang-dev \
    build-essential \
    musl-dev \
    gcc \
    clang \
    llvm-dev \
    libssl-dev \
    cmake \
    make \

    && rm -rf /var/lib/apt/lists/*#

# Copy binaries from all previous stages
COPY --from=lightwalletd-builder /usr/local/bin/lightwalletd /usr/local/bin/
COPY --from=zcash-builder /usr/local/bin/zcashd /usr/local/bin/
COPY --from=zcash-builder /usr/local/bin/zcash-cli /usr/local/bin/
COPY --from=zebrad-release /usr/local/bin/zebrad /usr/local/bin/

# Set the PATH
ENV PATH="/usr/local/bin:${PATH}"

# Expose any required ports here
# EXPOSE <port_number>

WORKDIR /app

# CMD ["cargo", "nextest", "run"]

