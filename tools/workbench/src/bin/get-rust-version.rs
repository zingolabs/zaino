//! Emit the pinned rustc version read from `rust-toolchain.toml`.
//!
//! Single source of truth for `RUST_VERSION` (CI image build, the GitHub
//! workflows, `Makefile.toml`). Exits non-zero if the channel is anything but a
//! concrete numeric version — `stable`/`nightly`/dated pins would make the
//! container image tag non-reproducible.

use workbench::{repo_root, run, toolchain_channel};

fn main() {
    run(
        "get-rust-version",
        || toolchain_channel(&repo_root()?),
        |version| println!("{version}"),
    )
}
