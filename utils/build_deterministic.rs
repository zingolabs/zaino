#!/usr/bin/env rust-script
//! Build the zainod OCI image and extract its binary reproducibly.
//!
//! Ported from the former `utils/build_deterministic.sh`. Runs two
//! `docker build` invocations against `Dockerfile.deterministic` with a
//! pinned platform and `SOURCE_DATE_EPOCH`, forwarding any extra arguments
//! to both builds.
#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

const PLATFORM: &str = "linux/amd64";

fn main() -> Result<(), Box<dyn Error>> {
    let repo_root = repo_root()?;
    let dockerfile = repo_root.join("Dockerfile.deterministic");
    let oci_output = repo_root.join("build/oci");

    // Extra arguments are forwarded verbatim to both builds.
    let forwarded: Vec<String> = env::args().skip(1).collect();

    std::fs::create_dir_all(&oci_output)?;

    // Build runtime image for `docker run`.
    println!("Building runtime image...");
    let oci_dest = format!(
        "type=oci,rewrite-timestamp=true,force-compression=true,dest={}/zainod.tar,name=zainod",
        oci_output.display()
    );
    deterministic_build(
        &dockerfile,
        &repo_root,
        &["--target", "runtime", "--output", &oci_dest],
        &forwarded,
    )?;

    // Extract binary locally from the export stage.
    println!("Extracting binary...");
    let local_dest = format!("type=local,dest={}/build", repo_root.display());
    deterministic_build(
        &dockerfile,
        &repo_root,
        &["--quiet", "--target", "export", "--output", &local_dest],
        &forwarded,
    )?;

    Ok(())
}

/// Run `docker build` against the deterministic Dockerfile with the flags
/// every build shares. Per-build flags and any caller-forwarded args follow.
fn deterministic_build(
    dockerfile: &Path,
    repo_root: &Path,
    per_build: &[&str],
    forwarded: &[String],
) -> Result<(), Box<dyn Error>> {
    let status = Command::new("docker")
        .arg("build")
        .arg("-f")
        .arg(dockerfile)
        .arg(repo_root)
        .arg("--platform")
        .arg(PLATFORM)
        .args(per_build)
        .args(forwarded)
        // Set per-command rather than mutating the process environment, which
        // would require `unsafe` under the 2024 edition.
        .env("DOCKER_BUILDKIT", "1")
        .env("SOURCE_DATE_EPOCH", "1")
        .status()?;

    if !status.success() {
        return Err(format!("docker build failed: {status}").into());
    }
    Ok(())
}

/// Resolve the repository root via `git rev-parse --show-toplevel`.
fn repo_root() -> Result<PathBuf, Box<dyn Error>> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err("`git rev-parse --show-toplevel` failed (not a git repository?)".into());
    }
    let path = String::from_utf8(output.stdout)?;
    Ok(PathBuf::from(path.trim()))
}
