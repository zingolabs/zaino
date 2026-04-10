use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tonic_prost_build::{compile_protos, configure};

const COMPACT_FORMATS_PROTO: &str = "proto/compact_formats.proto";
const PROPOSAL_PROTO: &str = "proto/proposal.proto";
const SERVICE_PROTO: &str = "proto/service.proto";

fn protoc_available() -> bool {
    if env::var_os("PROTOC").is_some() {
        return true;
    }
    #[cfg(feature = "heavy")]
    if which::which("protoc").is_ok() {
        return true;
    }
    false
}

/// Copy a generated file into the source tree and force non-executable
/// permissions so the working tree doesn't drift on build.
fn copy_generated(src: &Path, dst: &str) -> io::Result<()> {
    fs::copy(src, dst)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dst)?.permissions();
        perms.set_mode(0o644);
        fs::set_permissions(dst, perms)?;
    }
    Ok(())
}

fn main() -> io::Result<()> {
    // Check and compile proto files if needed
    if Path::new(COMPACT_FORMATS_PROTO).exists() && protoc_available() {
        build()?;
    }

    Ok(())
}

fn build() -> io::Result<()> {
    let out: PathBuf = env::var_os("OUT_DIR")
        .expect("Cannot find OUT_DIR environment variable")
        .into();

    // Build the compact format types.
    compile_protos(COMPACT_FORMATS_PROTO)?;

    // Copy the generated types into the source tree so changes can be committed.
    copy_generated(
        &out.join("cash.z.wallet.sdk.rpc.rs"),
        "src/proto/compact_formats.rs",
    )?;

    // Build the gRPC types and client.
    configure()
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
        .compile_protos(&[SERVICE_PROTO], &["proto/"])?;

    // Build the proposal types.
    compile_protos(PROPOSAL_PROTO)?;

    // Copy the generated types into the source tree so changes can be committed.
    copy_generated(
        &out.join("cash.z.wallet.sdk.ffi.rs"),
        "src/proto/proposal.rs",
    )?;

    // Copy the generated types into the source tree so changes can be committed. The
    // file has the same name as for the compact format types because they have the
    // same package, but we've set things up so this only contains the service types.
    copy_generated(
        &out.join("cash.z.wallet.sdk.rpc.rs"),
        "src/proto/service.rs",
    )?;

    Ok(())
}
