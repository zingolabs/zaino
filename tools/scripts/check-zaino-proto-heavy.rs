// Guard: `zaino-proto`'s `heavy` feature must stay enabled when a workspace is
// built with `--no-default-features`.
//
// The test suite runs with `--no-default-features` (the zcashd-off world;
// `zcashd_support` is opt-in and deprecating, docs/adr/0005). `heavy` is a
// *separate* default feature, on `zaino-proto` only, that
// pulls in zebra-state / zebra-chain / which. It survives `--no-default-features`
// only because every dependent pulls `zaino-proto` with default features (no
// `default-features = false` on those edges). If someone adds
// `default-features = false` to a `zaino-proto` dependency, `--no-default-features`
// would silently strip `heavy` and change the test build. This guard fails in
// that case.
//
// Run by the `check-zaino-proto-heavy` task via cargo-make's `@rust` runner.
// See docs/adr/0001-zcashd-support-feature-gate.md.
#![forbid(unsafe_code)]

use std::process::Command;

// Workspaces whose test suites run with --no-default-features, by manifest path.
const WORKSPACES: &[(&str, &str)] = &[
    ("production", "Cargo.toml"),
    ("live-tests", "live-tests/Cargo.toml"),
    ("e2e", "live-tests/e2e/Cargo.toml"),
];

// The feature node `cargo tree -e features` prints when `heavy` is enabled.
const HEAVY_NODE: &str = "zaino-proto feature \"heavy\"";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut failed = false;

    for (label, manifest) in WORKSPACES {
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
                 `heavy` (zebra-state / zebra-chain / which) from the no-zcashd test build that\n\
                 `makers container-test` / `live` use. Remove that\n\
                 `default-features = false`. See docs/adr/0001-zcashd-support-feature-gate.md.\n\
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
