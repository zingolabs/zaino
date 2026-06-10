{
  description = "Zaino — indexer and proxy server for the Zcash protocol";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  ### Build with Nix
  # nix build .#zainod — build the binary (output at `./result/bin/zainod`)
  # nix flake check — run fmt, clippy, doc, and the test suite
  # nix develop — enter a dev shell with the pinned Rust toolchain and build deps


  # TODO: nixConfig (extra-substituters + extra-trusted-public-keys)
  #       set once build-cache is setup

  outputs = { self, nixpkgs, flake-utils, crane, fenix }:
    let
      mkCraneLib = pkgs:
        (crane.mkLib pkgs).overrideToolchain (p:
          fenix.packages.${p.stdenv.buildPlatform.system}.fromToolchainFile {
            file = ./rust-toolchain.toml;
            sha256 = "sha256-gh/xTkxKHL4eiRXzWv8KP7vfjSk61Iq48x47BEDFgfk=";
          });

      overlay = final: _prev: {
        zainod = final.callPackage ./nix/package.nix {
          craneLib = mkCraneLib final;
        };
      };
    in
    {
      overlays.default = overlay;
    }
    // flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ overlay ];
        };

        craneLib = mkCraneLib pkgs;

        # self.rev is set on clean trees; self.dirtyRev (with a "-dirty" suffix) on dirty trees.
        zainod = pkgs.zainod.override {
          gitCommit = self.rev or self.dirtyRev;
        };

        # Single source of truth for src + build env lives in nix/package.nix
        # and is re-exposed through the derivation's passthru for downstream
        # checks. Avoids duplicating the source filter or env settings here.
        inherit (zainod.passthru) commonArgs cargoArtifacts;
      in
      {
        packages = {
          inherit zainod;
          default = zainod;
        };

        apps.default = {
          type = "app";
          program = "${zainod}/bin/zainod";
          meta = {
            inherit (zainod.meta) description;
          };
        };

        devShells.default = craneLib.devShell {
          packages = with pkgs; [
            protobuf
            pkg-config
            cmake
            rustPlatform.bindgenHook
            cargo-nextest
            cargo-deny
            cargo-make
            rust-analyzer
          ];

          inherit (commonArgs) env;
        };

        checks = {
          clippy = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--all-targets -- -D warnings";
          });

          fmt = craneLib.cargoFmt {
            inherit (commonArgs) src pname version;
          };

          nextest = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
          });

          doc = craneLib.cargoDoc (commonArgs // {
            inherit cargoArtifacts;
          });
        };

        formatter = pkgs.nixfmt-rfc-style;
      });
}
