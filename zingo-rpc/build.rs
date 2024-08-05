use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const COMPACT_FORMATS_PROTO: &str = "proto/compact_formats.proto";
const PROPOSAL_PROTO: &str = "proto/proposal.proto";
const SERVICE_PROTO: &str = "proto/service.proto";

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

    // Check and compile proto files if needed
    if Path::new(COMPACT_FORMATS_PROTO).exists()
        && env::var_os("PROTOC")
            .map(PathBuf::from)
            .or_else(|| which::which("protoc").ok())
            .is_some()
    {
        build()?;
    }

    Ok(())
}

fn build() -> io::Result<()> {
    let out: PathBuf = env::var_os("OUT_DIR")
        .expect("Cannot find OUT_DIR environment variable")
        .into();

    // Build the compact format types.
    tonic_build::compile_protos(COMPACT_FORMATS_PROTO)?;

    // Copy the generated types into the source tree so changes can be committed.
    fs::copy(
        out.join("cash.z.wallet.sdk.rpc.rs"),
        "src/proto/compact_formats.rs",
    )?;

    // Build the gRPC types and client.
    tonic_build::configure()
        .build_server(true)
        // .client_mod_attribute(
        //     "cash.z.wallet.sdk.rpc",
        //     r#"#[cfg(feature = "lightwalletd-tonic")]"#,
        // )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.ChainMetadata",
            "crate::proto::compact_formats::ChainMetadata",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactBlock",
            "crate::proto::compact_formats::CompactBlock",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactTx",
            "crate::proto::compact_formats::CompactTx",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactSaplingSpend",
            "crate::proto::compact_formats::CompactSaplingSpend",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactSaplingOutput",
            "crate::proto::compact_formats::CompactSaplingOutput",
        )
        .extern_path(
            ".cash.z.wallet.sdk.rpc.CompactOrchardAction",
            "crate::proto::compact_formats::CompactOrchardAction",
        )
        .compile(&[SERVICE_PROTO], &["proto/"])?;

    // Build the proposal types.
    tonic_build::compile_protos(PROPOSAL_PROTO)?;

    // Copy the generated types into the source tree so changes can be committed.
    fs::copy(
        out.join("cash.z.wallet.sdk.ffi.rs"),
        "src/proto/proposal.rs",
    )?;

    // Copy the generated types into the source tree so changes can be committed. The
    // file has the same name as for the compact format types because they have the
    // same package, but we've set things up so this only contains the service types.
    fs::copy(out.join("cash.z.wallet.sdk.rpc.rs"), "src/proto/service.rs")?;

    Ok(())
}
