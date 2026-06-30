#!/usr/bin/env rust-script
//! Guard: the deterministic build's rustc must match the canonical toolchain.
//!
//! Asserts that the `stagex/pallet-rust:<tag>` base image pinned in
//! `Dockerfile.deterministic` equals the `channel` in `rust-toolchain.toml`
//! (as surfaced by `tools/scripts/get-rust-version.rs`). These are two
//! independent version authorities — the canonical toolchain, and a
//! dependabot-managed docker base image — that otherwise drift silently; this
//! guard turns any drift into a build failure. Exits non-zero on mismatch.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, exit};

const PROG: &str = "check-toolchain-pin";
const DOCKERFILE: &str = "Dockerfile.deterministic";
const GET_RUST_VERSION: &str = "tools/scripts/get-rust-version.rs";

fn main() {
    match check() {
        Ok(version) => {
            println!("{PROG}: ok — pallet-rust and rust-toolchain.toml both pin {version}");
        }
        Err(lines) => {
            for line in lines {
                eprintln!("{PROG}: {line}");
            }
            exit(1);
        }
    }
}

fn check() -> Result<String, Vec<String>> {
    let root = repo_root()?;
    let canonical = canonical_channel(&root)?;
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

/// Canonical channel, via the existing single-source script (which also
/// validates that the channel is a concrete numeric version).
fn canonical_channel(root: &Path) -> Result<String, Vec<String>> {
    let script = root.join(GET_RUST_VERSION);
    // Invoke through `rust-script` rather than the file's shebang so this does
    // not depend on the nested script's executable bit.
    let output = Command::new("rust-script")
        .arg(&script)
        .output()
        .map_err(|e| vec![format!("failed to run {}: {e}", script.display())])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(vec![format!("{GET_RUST_VERSION} failed: {}", stderr.trim())]);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The `<tag>` from `FROM stagex/pallet-rust:<tag>@sha256:...` in the
/// deterministic Dockerfile.
fn pallet_rust_tag(root: &Path) -> Result<String, Vec<String>> {
    let path = root.join(DOCKERFILE);
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| vec![format!("cannot read {}: {e}", path.display())])?;
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

/// Repository root via `git rev-parse --show-toplevel`.
fn repo_root() -> Result<PathBuf, Vec<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| vec![format!("failed to run git: {e}")])?;
    if !output.status.success() {
        return Err(vec![
            "`git rev-parse --show-toplevel` failed (not a git repository?)".to_string(),
        ]);
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}
