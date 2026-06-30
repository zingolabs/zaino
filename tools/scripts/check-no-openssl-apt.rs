#!/usr/bin/env rust-script
//! Guard: no OpenSSL system package may be installed by any Dockerfile.
//!
//! The Rust dependency graph is kept OpenSSL-free by `deny.toml` (the `openssl`
//! / `openssl-sys` / `boring*` crate bans), but that policy cannot see `apt`
//! installs in our container images. This guard closes that gap: it scans every
//! tracked Dockerfile/Containerfile and fails if any line installs a `libssl*`
//! or `openssl*` package, exiting non-zero with the offending `file:line`.
//!
//! Companion to `deny.toml` (crates) — together they enforce "no OpenSSL".
//! TLS is rustls throughout; nothing here needs system OpenSSL.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::{Command, exit};

const PROG: &str = "check-no-openssl-apt";

fn main() {
    match check() {
        Ok(n) => println!("{PROG}: ok — no OpenSSL apt packages in {n} Dockerfile(s)"),
        Err(lines) => {
            for line in lines {
                eprintln!("{PROG}: {line}");
            }
            exit(1);
        }
    }
}

fn check() -> Result<usize, Vec<String>> {
    let root = repo_root()?;
    let files = dockerfiles(&root)?;

    let mut offenders = Vec::new();
    for rel in &files {
        let contents = std::fs::read_to_string(root.join(rel))
            .map_err(|e| vec![format!("cannot read {rel}: {e}")])?;
        for (i, line) in contents.lines().enumerate() {
            if is_openssl_package_line(line) {
                offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }

    if !offenders.is_empty() {
        let mut msg = vec![
            "OpenSSL system package(s) found in a Dockerfile — TLS is rustls, none is needed:"
                .to_string(),
        ];
        msg.extend(offenders);
        msg.push("remove the package, or justify it and update this guard.".to_string());
        return Err(msg);
    }
    Ok(files.len())
}

/// A non-comment line that installs a `libssl*` / `openssl*` apt package.
fn is_openssl_package_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return false; // prose mentioning openssl is fine
    }
    line.contains("libssl") || line.contains("openssl")
}

/// Tracked files whose name contains `Dockerfile` or `Containerfile`.
fn dockerfiles(root: &PathBuf) -> Result<Vec<String>, Vec<String>> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["ls-files"])
        .output()
        .map_err(|e| vec![format!("failed to run git ls-files: {e}")])?;
    if !output.status.success() {
        return Err(vec!["`git ls-files` failed".to_string()]);
    }
    let files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|p| {
            let name = p.rsplit('/').next().unwrap_or(p);
            name.contains("Dockerfile") || name.contains("Containerfile")
        })
        .map(str::to_string)
        .collect();
    Ok(files)
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
