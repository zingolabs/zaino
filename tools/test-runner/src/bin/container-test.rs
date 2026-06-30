//! `container-test` — run the test suite inside the local CI container via podman.
//!
//! Invoked from the `container-test` task (Makefile.toml) as
//! `cargo run --bin container-test -- <args>`. The container's entrypoint.sh sets
//! up the validator binaries (zcashd, zebrad, zcash-cli) by symlinking
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
//! Feature selection (docs/adr/0005): `zcashd_support` is opt-in, not a default.
//! The run is always `--no-default-features` (the zcashd-off world CI builds);
//! passing `--with-zcashd` additionally enables `--features zcashd_support`.
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

    // Pull `--with-zcashd` out of the forwarded args; everything else (e.g.
    // `-p clientless`) passes through to `cargo nextest run`.
    let mut with_zcashd = false;
    let mut forwarded: Vec<String> = Vec::new();
    for arg in env::args().skip(1) {
        if arg == "--with-zcashd" {
            with_zcashd = true;
        } else {
            forwarded.push(arg);
        }
    }

    // Always build the no-zcashd world (`--no-default-features`, matching CI);
    // `--with-zcashd` adds the opt-in feature back (docs/adr/0005).
    let mut feature_args: Vec<String> = vec!["--no-default-features".to_string()];
    if with_zcashd {
        feature_args.push("--features".to_string());
        feature_args.push("zcashd_support".to_string());
        info("-- zcashd_support    = ON (--features zcashd_support)");
    } else {
        info("-- zcashd_support    = OFF (--no-default-features)");
    }

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
    let mut argv: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--init".into(),
        "--pids-limit=-1".into(),
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
