//! `container-test` — run the test suite inside the local CI container via podman.
//!
//! Invoked from the `container-test` task (Makefile.toml) as
//! `cargo run --bin container-test -- <args>`. The container's entrypoint.sh sets
//! up the validator binaries (zebrad, zcash-devtool) by symlinking
//! `$TEST_BINARIES_DIR` into the expected location.
//!
//! Inputs from the environment (the Makefile `[env]` block exports the first
//! two; the rest are optional):
//!   IMAGE_NAME         container image repository (required)
//!   TEST_BINARIES_DIR  in-container artifacts dir (required)
//!   ZAINOLOG_FORMAT    log format forwarded into the container (default `stream`)
//!   RUST_LOG           log filter forwarded into the container (default empty)
//! TAG is computed here via tools/scripts/get-ci-image-tag.sh (it is a shell
//! variable in the base-script pre-script, not exported, so we recompute it).
//!
//! The run is always `--no-default-features`, matching what CI builds.
//! Any other arguments (e.g. `-p clientless`, `--test-threads 6`) pass straight
//! through to `cargo nextest run`.
//!
//! podman is run in the foreground with `--rm --init`, so Ctrl-C is forwarded to
//! the container and `--rm` tears it down — no manual cleanup trap needed.
#![forbid(unsafe_code)]

use std::env;
use std::error::Error;
use std::process::Command;

fn info(msg: &str) {
    println!("\x1b[1;36m\x1b[1m>>> {msg}\x1b[0m");
}

fn required(key: &str) -> Result<String, Box<dyn Error>> {
    env::var(key).map_err(|_| format!("{key} must be set (provided by the Makefile [env] block)").into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let image_name = required("IMAGE_NAME")?;
    let test_binaries_dir = required("TEST_BINARIES_DIR")?;
    let zainolog_format = env::var("ZAINOLOG_FORMAT").unwrap_or_else(|_| "stream".to_string());
    let rust_log = env::var("RUST_LOG").unwrap_or_default();

    // TAG is not exported into our environment, so compute it the same way the
    // shell pre-script did.
    let tag_out = Command::new("./tools/scripts/get-ci-image-tag.sh").output()?;
    if !tag_out.status.success() {
        return Err("get-ci-image-tag.sh failed".into());
    }
    let tag = String::from_utf8(tag_out.stdout)?.trim().to_string();

    // Everything (e.g. `-p clientless`) passes through to `cargo nextest run`.
    let forwarded: Vec<String> = env::args().skip(1).collect();

    // Always build with `--no-default-features` (matching CI).
    let feature_args: Vec<String> = vec!["--no-default-features".to_string()];

    info(&format!("-- IMAGE             = {image_name}"));
    info(&format!("-- TAG               = {tag}"));

    // Suffix the container name with our PID so concurrent runs on one host
    // don't collide on the name.
    let container_name = format!("zaino-testing-{}", std::process::id());
    let cwd = env::current_dir()?;
    let cwd = cwd.to_str().ok_or("current dir is not valid UTF-8")?;
    let image_ref = format!("{image_name}:{tag}");

    // `--pids-limit=-1` removes the default 2048-process cgroup cap: under a
    // num-cpus profile each test spawns a full zebrad whose rayon pool also
    // sizes to num-cpus, so peak task count scales ~num_cpus^2 and breaches the
    // default cap on many-core hosts (EAGAIN / rayon ThreadPoolBuildError).
    //
    // `--tmpfs /tmp` keeps the live suite's scratch off the container's overlay
    // layer.
    //
    // Why it is needed: the validator launcher polls the spawned validator's
    // captured stdout for its readiness line, re-`open`ing that file every
    // 50-100ms while a drainer thread appends the validator's debug output to
    // it. On a host running IMA appraisal (which the kernel enables
    // automatically under Secure Boot), IMA caches its verdict per inode, but
    // overlayfs inodes do not carry stable identity, so every one of those
    // opens misses the cache and re-hashes the whole file. Against a
    // continuously growing log, several tests at once, the `openat` calls wedge
    // in uninterruptible sleep (`ima_file_check` -> `process_measurement`) and
    // the test hangs until the nextest timeout kills it. tmpfs inodes are
    // stable and IMA policies do not appraise tmpfs, so the loop stays cheap.
    //
    // Why it is safe: everything the suite puts in /tmp is per-run scratch —
    // generated validator configs, validator data directories, and the captured
    // stdout/stderr logs — all of which are already discarded with the
    // container, which runs `--rm`. Nothing that has to outlive the run is
    // written there: the repository is bind-mounted at
    // /home/container_user/zaino and build output goes to the named target
    // volume. The mount keeps the default `exec` behaviour of the directory it
    // replaces so build and link steps that use TMPDIR are unaffected, and it
    // is left at the kernel's default size cap (half of RAM) rather than a
    // guessed limit, so a large run fails no earlier than it does today.
    // Hosts without IMA appraisal are unaffected apart from faster scratch I/O.
    let mut argv: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--init".into(),
        "--pids-limit=-1".into(),
        "--tmpfs".into(),
        // `size` must stay set: podman caps a `--tmpfs` mount at 64 MiB rather
        // than inheriting the kernel's tmpfs default. That starves the tests
        // that build LMDB databases in `tempfile` directories under /tmp.
        // tmpfs occupies only the memory actually written, so this reserves
        // nothing up front.
        "/tmp:rw,exec,nosuid,nodev,size=8g".into(),
        "--name".into(),
        container_name,
        "-v".into(),
        format!("{cwd}:/home/container_user/zaino"),
        "-v".into(),
        "zaino-container-target:/home/container_user/zaino/target:U".into(),
        "-v".into(),
        "zaino-cargo-git:/usr/local/cargo/git:U".into(),
        "-v".into(),
        "zaino-cargo-registry:/usr/local/cargo/registry:U".into(),
        "-e".into(),
        format!("TEST_BINARIES_DIR={test_binaries_dir}"),
        "-e".into(),
        "NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1".into(),
        "-e".into(),
        format!("ZAINOLOG_FORMAT={zainolog_format}"),
        "-e".into(),
        format!("RUST_LOG={rust_log}"),
        "-w".into(),
        "/home/container_user/zaino".into(),
        "-u".into(),
        "container_user".into(),
        image_ref,
        "cargo".into(),
        "nextest".into(),
        "run".into(),
    ];
    argv.extend(feature_args);
    argv.extend(forwarded);

    let status = Command::new("podman").args(&argv).status()?;
    std::process::exit(status.code().unwrap_or(1));
}
