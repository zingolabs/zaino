use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use tonic_prost_build::configure;

const COMPACT_FORMATS_PROTO: &str = "proto/compact_formats.proto";
const SERVICE_PROTO: &str = "proto/service.proto";
const ZEBRA_INDEXER_PROTO: &str = "proto/zebra_indexer.proto";
const PROTOCOL_CHANGELOG: &str = "lightwallet-protocol/CHANGELOG.md";

/// Service's compact-format types → `proto::compact_formats` (never generated twice)
const COMPACT_FORMAT_TYPES: [&str; 6] = [
    "ChainMetadata",
    "CompactBlock",
    "CompactTx",
    "CompactSaplingSpend",
    "CompactSaplingOutput",
    "CompactOrchardAction",
];

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

/// `OUT_DIR/<module>`, created (compact formats + service share one package = one file name)
fn module_dir(module: &str) -> io::Result<PathBuf> {
    let out = env::var_os("OUT_DIR").expect("cargo sets OUT_DIR for build scripts");
    let dir = PathBuf::from(out).join(module);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={COMPACT_FORMATS_PROTO}");
    println!("cargo:rerun-if-changed={SERVICE_PROTO}");
    println!("cargo:rerun-if-changed={ZEBRA_INDEXER_PROTO}");
    println!("cargo:rerun-if-changed={PROTOCOL_CHANGELOG}");

    let version = vendored_protocol_version(&fs::read_to_string(PROTOCOL_CHANGELOG)?)?;
    println!("cargo:rustc-env=LIGHTWALLET_PROTOCOL_VERSION={version}");

    configure()
        .out_dir(module_dir("compact_formats")?)
        .compile_protos(&[COMPACT_FORMATS_PROTO], &["proto/"])?;

    // - client only (zaino-grpc dispatches by path, no tonic server)
    // - `Bytes`: one tx to many streams = refcount bump, not a copy per client
    // - `LightdInfo` as lightwalletd's JSON names it (zainod's `/status` `info`)
    COMPACT_FORMAT_TYPES
        .iter()
        .fold(
            configure()
                .build_server(false)
                .out_dir(module_dir("service")?)
                .bytes(".cash.z.wallet.sdk.rpc.RawTransaction.data")
                .type_attribute(
                    ".cash.z.wallet.sdk.rpc.LightdInfo",
                    "#[derive(serde::Serialize)] #[serde(rename_all = \"camelCase\")]",
                ),
            |builder, name| {
                builder.extern_path(
                    format!(".cash.z.wallet.sdk.rpc.{name}"),
                    format!("crate::proto::compact_formats::{name}"),
                )
            },
        )
        .compile_protos(&[SERVICE_PROTO], &["proto/"])?;

    // zebrad's push streams + block fetch: Zaino = client, server half = tests' fake zebrad
    // - `Bytes`: a block's payload = a slice of the received frame, never copied out of it
    configure()
        .out_dir(module_dir("zebra_indexer")?)
        .bytes(".zebra.indexer.rpc.BlockAndHash.data")
        .compile_protos(&[ZEBRA_INDEXER_PROTO], &["proto/"])?;

    Ok(())
}
