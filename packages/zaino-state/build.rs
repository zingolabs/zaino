use std::env;
use std::io;
use std::process::Command;

fn git(args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .output()
        .expect("git failed")
        .stdout;
    String::from_utf8(out)
        .expect("git output not UTF-8")
        .trim()
        .to_string()
}

fn main() -> io::Result<()> {
    // Without these, cargo's default is "rerun if any file in the package
    // changes", which combined with wall-clock-derived rustc-env values
    // would invalidate this crate (and everything downstream) on every build.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rerun-if-env-changed=GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=BRANCH");

    // Prefer caller-supplied values: the makers container-test task computes
    // these on the host and passes them via `-e`, so the in-container build
    // does not depend on a working git inside the bind-mounted workspace
    // (which is broken across worktree gitdir indirection). Fall back to
    // shelling out for direct host invocations of `cargo build`.
    let git_commit = env::var("GIT_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| git(&["rev-parse", "HEAD"]));
    println!("cargo:rustc-env=GIT_COMMIT={git_commit}");

    let branch = env::var("BRANCH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| git(&["rev-parse", "--abbrev-ref", "HEAD"]));
    println!("cargo:rustc-env=BRANCH={branch}");

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

    Ok(())
}
