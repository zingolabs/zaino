//! Guard: the deterministic build's rustc must match the canonical toolchain.
//!
//! Asserts that `stagex/pallet-rust:<tag>` in `Dockerfile.deterministic` equals
//! `rust-toolchain.toml`'s `channel`. These are two independent version
//! authorities — the canonical toolchain and a dependabot-managed docker base
//! image — that otherwise drift silently; this turns drift into a build failure.

use std::path::Path;
use workbench::{read, repo_root, run, toolchain_channel};

const DOCKERFILE: &str = "Dockerfile.deterministic";

fn main() {
    run("check-toolchain-pin", check, |version| {
        println!(
            "check-toolchain-pin: ok — pallet-rust and rust-toolchain.toml both pin {version}"
        );
    })
}

fn check() -> Result<String, Vec<String>> {
    let root = repo_root()?;
    let canonical = toolchain_channel(&root)?;
    let pinned = pallet_rust_tag(&root)?;

    if canonical != pinned {
        return Err(vec![
            format!(
                "toolchain skew: {DOCKERFILE} pins stagex/pallet-rust:{pinned}, but \
                 rust-toolchain.toml channel is {canonical}"
            ),
            format!(
                "align them: set the pallet-rust tag (and its digest) to {canonical}, \
                 or bump rust-toolchain.toml to {pinned}"
            ),
        ]);
    }
    Ok(canonical)
}

/// The `<tag>` from `FROM stagex/pallet-rust:<tag>@sha256:...`.
fn pallet_rust_tag(root: &Path) -> Result<String, Vec<String>> {
    let path = root.join(DOCKERFILE);
    let contents = read(&path)?;
    contents
        .lines()
        .find_map(|line| {
            let after = line.split_once("stagex/pallet-rust:")?.1;
            let tag = after.split_once('@')?.0;
            (!tag.is_empty()).then(|| tag.to_string())
        })
        .ok_or_else(|| {
            vec![format!(
                "no `FROM stagex/pallet-rust:<tag>@...` line in {}",
                path.display()
            )]
        })
}
