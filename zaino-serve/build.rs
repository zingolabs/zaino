use std::env;
use std::io;
use std::process::Command;

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
    let build_date = Command::new("date")
        .output()
        .expect("Failed to get build date")
        .stdout;
    let build_date = String::from_utf8(build_date).expect("Invalid UTF-8 sequence");
    println!("cargo:rustc-env=BUILD_DATE={}", build_date.trim());

    // Set the build user
    let build_user = whoami::username();
    println!("cargo:rustc-env=BUILD_USER={}", build_user);

    // Set the version from Cargo.toml
    let version = env::var("CARGO_PKG_VERSION").expect("Failed to get version from Cargo.toml");
    println!("cargo:rustc-env=VERSION={}", version);

    Ok(())
}
