---
default: patch
---

# `zainod`: The image drops its `HEALTHCHECK` and builds on the pinned toolchain

The container image no longer declares a `HEALTHCHECK` (it ran `zainod --version`, which proves nothing about serving); probe `/livez` and `/readyz` on the admin listener (`[metrics] listen_address`) instead. The builder stage uses `rust:1.96.0`, the toolchain `rust-toolchain.toml` pins.
