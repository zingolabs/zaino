use std::env;
use std::io;
use std::process::Command;

fn main() -> io::Result<()> {
    // Without these, cargo's default is "rerun if any file in the package
    // changes", which combined with wall-clock-derived rustc-env values
    // would invalidate this crate (and everything downstream) on every build.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    // Fetch the commit hash
    let commit_hash = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("Failed to get commit hash")
        .stdout;
    let commit_hash = String::from_utf8(commit_hash).expect("Invalid UTF-8 sequence");
    println!("cargo:rustc-env=GIT_COMMIT={}", commit_hash.trim());

    // Fetch the current branch
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("Failed to get branch")
        .stdout;
    let branch = String::from_utf8(branch).expect("Invalid UTF-8 sequence");
    println!("cargo:rustc-env=BRANCH={}", branch.trim());

    // BUILD_DATE: SOURCE_DATE_EPOCH if set
    // (https://reproducible-builds.org/docs/source-date-epoch/), otherwise
    // a fixed sentinel. Never wall-clock — that value would differ on every
    // run and force rustc to rebuild this crate every time.
    let build_date = env::var("SOURCE_DATE_EPOCH")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_DATE={}", build_date);

    // Set the build user
    let build_user = whoami::username();
    println!("cargo:rustc-env=BUILD_USER={build_user}");

    // Set the version from Cargo.toml
    let version = env::var("CARGO_PKG_VERSION").expect("Failed to get version from Cargo.toml");
    println!("cargo:rustc-env=VERSION={version}");

    Ok(())
}
