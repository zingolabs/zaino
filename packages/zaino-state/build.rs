use std::env;
use std::io;
use std::path::PathBuf;
use std::process::Command;

fn git_ok(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() -> io::Result<()> {
    // Without these, cargo's default is "rerun if any file in the package
    // changes", which combined with wall-clock-derived rustc-env values
    // would invalidate this crate (and everything downstream) on every build.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    // Watch HEAD via the real gitdir. `git rev-parse --git-dir` returns
    // `../../.git` in a normal checkout and the absolute linked-gitdir under
    // `<main>/.git/worktrees/<name>` in a worktree. A literal
    // `../../.git/HEAD` does not exist in the worktree case (where `.git` is
    // a pointer file), and a missing rerun-if-changed target makes cargo
    // treat the script as perpetually changed — rerunning it and cascading
    // a recompile through everything downstream on every invocation.
    //
    // If git isn't usable (e.g. building inside a container that bind-mounts
    // the worktree without its sibling gitdir), skip the watcher entirely
    // rather than registering a non-existent path.
    if let Some(head) = git_ok(&["rev-parse", "--git-dir"])
        .map(|d| PathBuf::from(d).join("HEAD"))
        .filter(|p| p.exists())
    {
        println!("cargo:rerun-if-changed={}", head.display());
    }

    // Stable fallbacks when git is unreachable — the rustc fingerprint must
    // not differ between back-to-back invocations in the same context.
    let git_commit = git_ok(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let branch =
        git_ok(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIT_COMMIT={git_commit}");
    println!("cargo:rustc-env=BRANCH={branch}");

    // BUILD_DATE: SOURCE_DATE_EPOCH if set
    // (https://reproducible-builds.org/docs/source-date-epoch/), otherwise
    // a fixed sentinel. Never wall-clock — that value would differ on every
    // run and force rustc to rebuild this crate every time.
    let build_date = env::var("SOURCE_DATE_EPOCH")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_DATE={build_date}");

    let build_user = whoami::username();
    println!("cargo:rustc-env=BUILD_USER={build_user}");

    Ok(())
}
