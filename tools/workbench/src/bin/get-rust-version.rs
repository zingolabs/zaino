//! Emit the pinned rustc version read from `rust-toolchain.toml`.
//!
//! `RUST_VERSION` build-arg for the release image (`FROM rust:${RUST_VERSION}-bookworm`)
//! - non-numeric channel = error (no matching pinned base image)

use workbench::{repo_root, run, toolchain_channel};

fn main() {
    run(
        "get-rust-version",
        || toolchain_channel(&repo_root()?),
        |version| println!("{version}"),
    )
}
