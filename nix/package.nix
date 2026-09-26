{ lib
, stdenv
, craneLib
, rustPlatform
, autoPatchelfHook
, protobuf
, pkg-config
, cmake
, withTls ? true
, gitCommit ? "unknown"
, gitBranch ? "unknown"
}:

let
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      (craneLib.fileset.commonCargoSources ../.)
      # commonCargoSources only includes .rs & cargo files
      #   .proto — read by tonic-build (zaino-proto/build.rs)
      (lib.fileset.fileFilter (f: f.hasExt "proto") ../packages/zaino-proto)
    ];
  };

  crateInfo = craneLib.crateNameFromCargoToml {
    cargoToml = ../packages/zainod/Cargo.toml;
  };

  commonArgs = {
    inherit src;
    inherit (crateInfo) pname version;

    strictDeps = true;
    doCheck = false;

    nativeBuildInputs = [
      protobuf
      pkg-config
      cmake
      autoPatchelfHook
    ];

    # stdenv.cc.cc.lib provides the libgcc_s.so.1 that Rust binaries on
    # linux-gnu load at runtime.
    buildInputs = [ stdenv.cc.cc.lib ];

    env = {
      PROTOC = "${protobuf}/bin/protoc";
      PROTOC_INCLUDE = "${protobuf}/include";

      ZAINO_GIT_COMMIT_ID = gitCommit;
      ZAINO_GIT_BRANCH = gitBranch;
    };
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (commonArgs // {
  inherit cargoArtifacts;

  cargoExtraArgs =
    "--locked --package zainod --bin zainod"
    + lib.optionalString (!withTls) " --features no_tls_use_unencrypted_traffic";

  passthru = {
    inherit commonArgs;
  };

  meta = {
    description = "Indexer and proxy server for the Zcash protocol";
    homepage = "https://github.com/zingolabs/zaino";
    license = lib.licenses.asl20;
    mainProgram = "zainod";
    platforms = lib.platforms.unix;
  };
})
