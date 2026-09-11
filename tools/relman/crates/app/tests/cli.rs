//! Binary-level tests: each runs the built `relman` against a throwaway
//! fixture on disk and asserts on the exit status and output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run `relman <args>` with `dir` as the working directory.
fn relman(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relman"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("relman runs")
}

/// Run `git <args>` in `dir` with a throwaway identity and no ambient config.
fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "relman-test")
        .env("GIT_AUTHOR_EMAIL", "relman-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "relman-test")
        .env("GIT_COMMITTER_EMAIL", "relman-test@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run `cargo <args>` offline against the fixture workspace at `root`.
fn cargo(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO"))
        .current_dir(root)
        .args(args)
        .arg("--offline")
        .output()
        .expect("cargo runs")
}

/// A two-crate fixture workspace: `beta` depends on `alpha` through the root
/// `[workspace.dependencies]` pin, and `relman.toml` governs both.
fn write_fixture_workspace(root: &Path) {
    fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"alpha\", \"beta\"]\n\n\
         [workspace.dependencies]\nalpha = { path = \"alpha\", version = \"0.1.0\" }\n",
    )
    .expect("write root manifest");
    write_member(
        root,
        "alpha",
        "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_member(
        root,
        "beta",
        "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nalpha = { workspace = true }\n",
    );
    fs::write(
        root.join("relman.toml"),
        "[[target]]\nname = \"alpha\"\npath = \"alpha\"\n\n\
         [[target]]\nname = \"beta\"\npath = \"beta\"\n",
    )
    .expect("write relman.toml");
    fs::create_dir_all(root.join(".changesets")).expect("mkdir .changesets");
}

fn write_member(root: &Path, dir: &str, manifest: &str) {
    let crate_dir = root.join(dir);
    fs::create_dir_all(crate_dir.join("src")).expect("mkdir member");
    fs::write(crate_dir.join("Cargo.toml"), manifest).expect("write member manifest");
    fs::write(crate_dir.join("src/lib.rs"), "").expect("write member lib");
}

fn commit_all(root: &Path, message: &str) {
    git(root, &["add", "-A"]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-q", "-m", message],
    );
}

/// `relman about` reports the tool's own version and the clock; it needs no
/// `relman.toml`, no ledger, and no repository, so it must succeed anywhere
/// `relman --version` does.
#[test]
fn about_runs_outside_a_relman_repo() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = relman(dir.path(), &["about"]);
    assert!(
        output.status.success(),
        "relman about failed outside a repo:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("relman:"));
}

/// A malformed changeset and an uncovered target are different failures: CI
/// must always fail the former and may only warn on the latter during the
/// advisory rollout, so the exit status has to tell them apart.
#[test]
fn changeset_check_distinguishes_malformed_from_uncovered() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    write_fixture_workspace(root);
    git(root, &["init", "-q", "-b", "dev"]);
    commit_all(root, "base");

    // Branch one: touches governed source, adds no changeset.
    git(root, &["checkout", "-q", "-b", "uncovered"]);
    fs::write(root.join("alpha/src/lib.rs"), "pub fn a() {}\n").expect("touch alpha");
    commit_all(root, "touch alpha");
    let uncovered = relman(root, &["changeset", "check"]);
    assert!(!uncovered.status.success(), "uncovered target must fail");

    // Branch two: same source change, plus a changeset that fails to parse.
    git(root, &["checkout", "-q", "dev"]);
    git(root, &["checkout", "-q", "-b", "malformed"]);
    fs::write(root.join("alpha/src/lib.rs"), "pub fn a() {}\n").expect("touch alpha");
    fs::write(
        root.join(".changesets/pr-1.toml"),
        "[[changes]]\ncrate = \"alpha\"\nkind = \"fix\"\ndescription = \"x\"\n\
         [empty]\nreason = \"y\"\n",
    )
    .expect("write malformed changeset");
    commit_all(root, "touch alpha with a malformed changeset");
    let malformed = relman(root, &["changeset", "check"]);
    assert!(!malformed.status.success(), "malformed changeset must fail");

    assert_ne!(
        uncovered.status.code(),
        malformed.status.code(),
        "a malformed changeset and an uncovered target exit with the same code, \
         so CI cannot always fail the former while only warning on the latter"
    );
}

/// `relman bump` rewrites crate versions, so the workspace lockfile it leaves
/// behind must still be consistent: the very next `cargo --locked` in the
/// release pipeline refuses to run otherwise.
#[test]
fn bump_leaves_the_lockfile_consistent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    write_fixture_workspace(root);
    fs::write(
        root.join(".changesets/pr-1.toml"),
        "[[changes]]\ncrate = \"alpha\"\nkind = \"feature\"\ndescription = \"A feature.\"\n",
    )
    .expect("write changeset");
    let generated = cargo(root, &["generate-lockfile"]);
    assert!(
        generated.status.success(),
        "generate-lockfile failed:\n{}",
        String::from_utf8_lossy(&generated.stderr)
    );

    let bumped = relman(root, &["bump"]);
    assert!(
        bumped.status.success(),
        "relman bump failed:\n{}",
        String::from_utf8_lossy(&bumped.stderr)
    );
    let alpha_manifest = fs::read_to_string(root.join("alpha/Cargo.toml")).expect("read alpha");
    assert!(
        !alpha_manifest.contains("version = \"0.1.0\""),
        "the fixture changeset should have bumped alpha"
    );

    let locked = cargo(root, &["metadata", "--format-version", "1", "--locked"]);
    assert!(
        locked.status.success(),
        "Cargo.lock is stale after `relman bump`:\n{}",
        String::from_utf8_lossy(&locked.stderr)
    );
}

/// Every relman crate root forbids unsafe code.
#[test]
fn every_crate_root_forbids_unsafe_code() {
    let crates_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let roots: Vec<PathBuf> = ["adapters", "cli", "config", "core", "domain"]
        .iter()
        .map(|name| crates_dir.join(name).join("src/lib.rs"))
        .chain(std::iter::once(crates_dir.join("app/src/main.rs")))
        .collect();
    let missing: Vec<String> = roots
        .iter()
        .filter(|root| {
            !fs::read_to_string(root)
                .expect("crate root is readable")
                .contains("#![forbid(unsafe_code)]")
        })
        .map(|root| root.display().to_string())
        .collect();
    assert!(
        missing.is_empty(),
        "crate roots without `#![forbid(unsafe_code)]`:\n{}",
        missing.join("\n")
    );
}
