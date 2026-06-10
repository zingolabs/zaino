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
, rocksdb_8_11
}:

let
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      (craneLib.fileset.commonCargoSources ../.)
      # commonCargoSources only includes .rs & cargo files
      #   .proto — read by tonic-build (zaino-proto/build.rs)
      #   .txt   — embedded via include_str! (db schema)
      #   .mmd   — embedded via simple_mermaid::mermaid! (doc diagrams)
      (lib.fileset.fileFilter (f: f.hasExt "proto") ../packages/zaino-proto)
      (lib.fileset.fileFilter (f: f.hasExt "txt" || f.hasExt "mmd") ../packages/zaino-state/src)
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
      # Sets LIBCLANG_PATH so librocksdb-sys's bindgen finds libclang
      # without dragging LLVM into the build.
      rustPlatform.bindgenHook
      autoPatchelfHook
    ];

    # stdenv.cc.cc.lib provides libstdc++.so.6 / libgcc_s.so.1 that
    # rocksdb's C++ code transitively needs at runtime.
    buildInputs = [ rocksdb_8_11 stdenv.cc.cc.lib ];

    env = {
      PROTOC = "${protobuf}/bin/protoc";
      PROTOC_INCLUDE = "${protobuf}/include";

      # Use nixpkgs' librocksdb instead of librocksdb-sys's bundled C++ compile.
      ROCKSDB_LIB_DIR = "${rocksdb_8_11}/lib";
      ROCKSDB_INCLUDE_DIR = "${rocksdb_8_11}/include";

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
    inherit cargoArtifacts commonArgs;
  };

  meta = {
    description = "Indexer and proxy server for the Zcash protocol";
    homepage = "https://github.com/zingolabs/zaino";
    license = lib.licenses.asl20;
    mainProgram = "zainod";
    platforms = lib.platforms.unix;
  };
})
