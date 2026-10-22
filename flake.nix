{
  description = "Zaino — Zcash indexer serving light wallets over lightwalletd-compatible gRPC";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    flake-utils.url = "github:numtide/flake-utils";

    crane.url = "github:ipetkov/crane";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  ### Build with Nix
  # nix build .#zainod — build the binary (output at `./result/bin/zainod`)
  # nix develop — enter a dev shell with the pinned Rust toolchain and build deps

  outputs = { self, nixpkgs, flake-utils, crane, fenix }:
    let
      mkCraneLib = pkgs:
        (crane.mkLib pkgs).overrideToolchain (p:
          fenix.packages.${p.stdenv.buildPlatform.system}.fromToolchainFile {
            file = ./rust-toolchain.toml;
            sha256 = "sha256-mvUGEOHYJpn3ikC5hckneuGixaC+yGrkMM/liDIDgoU=";
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

        zainod = pkgs.zainod;

        # Build env defined once in nix/package.nix, reused by devShell via passthru
        inherit (zainod.passthru) commonArgs;

        # crates.io tarball + its own Cargo.lock (not in nixpkgs)
        code-dupes = pkgs.rustPlatform.buildRustPackage rec {
          pname = "code-dupes";
          version = "0.2.1";
          src = pkgs.fetchCrate {
            inherit pname version;
            hash = "sha256-WUF++iDROO4mjA5FseT3FpFExbVx0GEBd+KylkpYWc0=";
          };
          cargoHash = "sha256-sv7jawb02hi4h2LPLoDJA10Rnp8YVz/L+X5GNcm4/+Y=";
          doCheck = false; # tests read ../cargo-dupes/tests/fixtures (absent from crates.io tarball)
        };
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
            cargo-nextest
            cargo-hack
            # changesets, version bump, changelog, GitHub release (knope.toml)
            knope
            rust-analyzer

            # .pre-commit-config.yaml hooks
            pre-commit
            cargo-deny
            code-dupes

            # zainod `[snapshot]` bootstrap + its e2e test
            aria2

            # Integration tests
            kind
            kubectl
          ];

          env = commonArgs.env;
        };

        formatter = pkgs.nixfmt-rfc-style;
      });
}
