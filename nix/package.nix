{ lib
, craneLib
, protobuf
, pkg-config
, makeWrapper
, aria2
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

  commonArgs = {
    inherit src;
    pname = "zainod";
    # lockstep workspace version (crateNameFromCargoToml can't follow `version.workspace = true` → 0.0.1)
    version = (lib.importTOML ../Cargo.toml).workspace.package.version;

    strictDeps = true;
    doCheck = false;
    cargoExtraArgs = "--locked --package zainod";

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

  # aria2c = the `[snapshot]` bootstrap's downloader
  nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ makeWrapper ];
  postInstall = ''
    wrapProgram $out/bin/zainod --prefix PATH : ${lib.makeBinPath [ aria2 ]}
  '';

  passthru = {
    inherit commonArgs;
  };

  meta = {
    description = "Zcash indexer serving light wallets over lightwalletd-compatible gRPC";
    homepage = "https://github.com/zingolabs/zaino";
    license = lib.licenses.asl20;
    mainProgram = "zainod";
    platforms = lib.platforms.unix;
  };
})
