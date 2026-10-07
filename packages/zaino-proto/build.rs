use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tonic_prost_build::{compile_protos, configure};

const COMPACT_FORMATS_PROTO: &str = "proto/compact_formats.proto";
const SERVICE_PROTO: &str = "proto/service.proto";
const ZEBRA_INDEXER_PROTO: &str = "proto/zebra_indexer.proto";
const PROTOCOL_CHANGELOG: &str = "lightwallet-protocol/CHANGELOG.md";

/// Newest released `## [vX.Y.Z]` heading (skips `[Unreleased]`)
fn vendored_protocol_version(changelog: &str) -> io::Result<String> {
    changelog
        .lines()
        .find_map(|line| line.strip_prefix("## [v")?.split_once(']'))
        .map(|(version, _)| format!("v{version}"))
        .ok_or_else(|| {
            io::Error::other(format!("no `## [vX.Y.Z]` heading in {PROTOCOL_CHANGELOG}"))
        })
}

fn protoc_available() -> bool {
    env::var_os("PROTOC").is_some() || which::which("protoc").is_ok()
}

/// Generated file → source tree, mode 0644 (no working-tree drift)
///
/// - byte-identical = no write (mtime kept, crate not re-invalidated)
fn copy_generated(src: &Path, dst: &str) -> io::Result<()> {
    let new = fs::read(src)?;
    if fs::read(dst).ok().as_deref() == Some(new.as_slice()) {
        return Ok(());
    }
    fs::write(dst, &new)?;
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
    // Explicit list (cargo's default = any package file, incl. the src/proto/*.rs written here)
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={COMPACT_FORMATS_PROTO}");
    println!("cargo:rerun-if-changed={SERVICE_PROTO}");
    println!("cargo:rerun-if-changed={ZEBRA_INDEXER_PROTO}");
    println!("cargo:rerun-if-changed={PROTOCOL_CHANGELOG}");

    let version = vendored_protocol_version(&fs::read_to_string(PROTOCOL_CHANGELOG)?)?;
    println!("cargo:rustc-env=LIGHTWALLET_PROTOCOL_VERSION={version}");

    if Path::new(COMPACT_FORMATS_PROTO).exists() && protoc_available() {
        build()?;
    }

    Ok(())
}

fn build() -> io::Result<()> {
    let out: PathBuf =
        env::var_os("OUT_DIR").expect("Cannot find OUT_DIR environment variable").into();

    compile_protos(COMPACT_FORMATS_PROTO)?;
    copy_generated(&out.join("cash.z.wallet.sdk.rpc.rs"), "src/proto/compact_formats.rs")?;

    // Service's compact-format types → the module compiled above
    const COMPACT_FORMAT_TYPES: [&str; 6] = [
        "ChainMetadata",
        "CompactBlock",
        "CompactTx",
        "CompactSaplingSpend",
        "CompactSaplingOutput",
        "CompactOrchardAction",
    ];
    COMPACT_FORMAT_TYPES
        .iter()
        .fold(
            configure()
                .build_server(true)
                // - `Bytes`: one tx to many streams = refcount bump, not a copy per client
                // - this field only (the one payload large + fanned out enough to matter)
                .bytes(".cash.z.wallet.sdk.rpc.RawTransaction.data"),
            |builder, name| {
                builder.extern_path(
                    format!(".cash.z.wallet.sdk.rpc.{name}"),
                    format!("crate::proto::compact_formats::{name}"),
                )
            },
        )
        .compile_protos(&[SERVICE_PROTO], &["proto/"])?;

    // zebrad's push streams: Zaino is a client; the server half = tests' fake zebrad
    configure().build_server(true).compile_protos(&[ZEBRA_INDEXER_PROTO], &["proto/"])?;
    copy_generated(&out.join("zebra.indexer.rpc.rs"), "src/proto/zebra_indexer.rs")?;

    // Same package name as compact formats → same file name; holds the service types only
    copy_generated(&out.join("cash.z.wallet.sdk.rpc.rs"), "src/proto/service.rs")?;

    Ok(())
}
