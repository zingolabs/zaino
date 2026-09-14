{
  description = "Zaino — indexer and proxy server for the Zcash protocol";

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


  # TODO: nixConfig (extra-substituters + extra-trusted-public-keys)
  #       set once build-cache is setup

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

        # self.rev is set on clean trees; self.dirtyRev (with a "-dirty" suffix) on dirty trees.
        zainod = pkgs.zainod.override {
          gitCommit = self.rev or self.dirtyRev;
        };

        # Build env defined once in nix/package.nix; devShell reuses it via passthru
        inherit (zainod.passthru) commonArgs;
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
            shellcheck
            rust-analyzer

            # Integration tests
            kind
            kubectl
            openshift
          ];

          env = commonArgs.env // {
            # Needed for librocksdb-sys
            LD_LIBRARY_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };
        };

        formatter = pkgs.nixfmt-rfc-style;
      });
}
