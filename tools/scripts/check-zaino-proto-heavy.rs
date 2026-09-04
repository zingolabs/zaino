// Guard: `zaino-proto`'s `heavy` feature must stay enabled when a workspace is
// built with `--no-default-features`.
//
// The test suite is archived with `--no-default-features`. `heavy` is a
// *separate* default feature, on `zaino-proto` only, that
// pulls in zebra-state / zebra-chain / which. It survives `--no-default-features`
// only because every dependent pulls `zaino-proto` with default features (no
// `default-features = false` on those edges). If someone adds
// `default-features = false` to a `zaino-proto` dependency, `--no-default-features`
// would silently strip `heavy` and change the test build. This guard fails in
// that case.
//
// Run by the `check-zaino-proto-heavy` task via cargo-make's `@rust` runner.
#![forbid(unsafe_code)]

use std::process::Command;

// Manifests whose test suites run with --no-default-features.
//
// Only the root manifest: it covers every production member, and those are the
// only crates whose `zaino-proto` edges this guard is about. The live-test
// crates are a separate standalone workspace (live-tests/Cargo.toml) that runs
// against deployed images and links no production crate, so there is no edge
// there to strip.
const MANIFESTS: &[(&str, &str)] = &[("production", "Cargo.toml")];

// The feature node `cargo tree -e features` prints when `heavy` is enabled.
const HEAVY_NODE: &str = "zaino-proto feature \"heavy\"";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut failed = false;

    for (label, manifest) in MANIFESTS {
        let out = Command::new("cargo")
            .args([
                "tree",
                "--manifest-path",
                manifest,
                "--no-default-features",
                "-e",
                "features",
                "-i",
                "zaino-proto",
            ])
            .output()?;

        if !out.status.success() {
            eprintln!(
                "[{label}] `cargo tree` failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            failed = true;
            continue;
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains(HEAVY_NODE) {
            println!(
                "[{label}] OK: zaino-proto `heavy` stays enabled under --no-default-features"
            );
        } else {
            eprintln!(
                "[{label}] FAIL: zaino-proto `heavy` is NOT enabled under --no-default-features.\n\
                 A `zaino-proto` dependency likely sets `default-features = false`, which strips\n\
                 `heavy` (zebra-state / zebra-chain / which) from the test build.\n\
                 Remove that\n\
                 `default-features = false`.\n\
                 --- cargo tree output ---\n{stdout}"
            );
            failed = true;
        }
    }

    if failed {
        return Err("zaino-proto `heavy` invariant violated under --no-default-features".into());
    }
    Ok(())
}
