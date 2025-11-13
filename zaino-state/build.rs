use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::str::FromStr as _;
use std::time::{SystemTime, UNIX_EPOCH};

use cargo_lock::package::GitReference;
use cargo_lock::package::SourceKind;
use cargo_lock::Lockfile;

fn main() -> io::Result<()> {
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

    // Set the build date
    // SOURCE_DATE_EPOCH can be used to set system time to a desired value
    // which is important for achieving determinism. More details can be found
    // at https://reproducible-builds.org/docs/source-date-epoch/
    let build_date = match env::var("SOURCE_DATE_EPOCH") {
        Ok(s) => s.trim().to_string(),
        Err(_) => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string(),
    };

    println!("cargo:rustc-env=BUILD_DATE={}", build_date);

    // Set the build user
    let build_user = whoami::username();
    println!("cargo:rustc-env=BUILD_USER={build_user}");

    // Set the version from Cargo.toml
    let version = env::var("CARGO_PKG_VERSION").expect("Failed to get version from Cargo.toml");
    println!("cargo:rustc-env=VERSION={version}");
    let lockfile = Lockfile::load("../Cargo.lock").expect("build script cannot load lockfile");
    let maybe_zebra_rev = lockfile.packages.iter().find_map(|package| {
        if package.name == cargo_lock::Name::from_str("zebra-chain").unwrap() {
            package
                .source
                .as_ref()
                .and_then(|source_id| match source_id.kind() {
                    SourceKind::Git(GitReference::Rev(rev)) => Some(rev),
                    _ => None,
                })
        } else {
            None
        }
    });
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("zebraversion.rs");
    fs::write(
        &dest_path,
        format!("const ZEBRA_VERSION: Option<&'static str> = {maybe_zebra_rev:?};"),
    )
    .unwrap();

    Ok(())
}
