{ lib
, craneLib
, protobuf
, pkg-config
  # zainod cargo features, e.g. [ "snapshot" ] (`[snapshot]` bootstrap; aria2c on PATH at runtime)
, features ? [ ]
}:

let
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      (craneLib.fileset.commonCargoSources ../.)
      # commonCargoSources only includes .rs & cargo files
      #   .proto — read by tonic-build (zaino-proto/build.rs)
      #   usage.md — crate docs via #![doc = include_str!("../usage.md")]
      #   lightwallet-protocol/CHANGELOG.md — protocol version (zaino-proto/build.rs)
      (lib.fileset.fileFilter (f: f.hasExt "proto") ../packages/zaino-proto)
      ../packages/zaino-proto/lightwallet-protocol/CHANGELOG.md
      (lib.fileset.fileFilter (f: f.name == "usage.md") ../packages)
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
    # deps built with the same features (else the deps-only cache misses them)
    cargoExtraArgs = "--locked --package zainod"
      + lib.optionalString (features != [ ]) " --features ${lib.concatStringsSep "," features}";

    nativeBuildInputs = [
      protobuf
      pkg-config
    ];

    env = {
      PROTOC = "${protobuf}/bin/protoc";
      PROTOC_INCLUDE = "${protobuf}/include";
    };
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (commonArgs // {
  inherit cargoArtifacts;

  cargoExtraArgs = "${commonArgs.cargoExtraArgs} --bin zainod";

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
