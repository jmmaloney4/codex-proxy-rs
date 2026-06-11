{
  description = "codex-proxy-rs scaffold";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    fenix,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [fenix.overlays.default];
      };
      toolchain = fenix.packages.${system}.stable.withComponents [
        "cargo"
        "clippy"
        "rust-src"
        "rustc"
        "rustfmt"
      ];
      rustAnalyzer = fenix.packages.${system}.rust-analyzer;
      rustPlatform = pkgs.makeRustPlatform {
        cargo = toolchain;
        rustc = toolchain;
      };
    in {
      devShells.default = pkgs.mkShell {
        packages = [
          toolchain
          rustAnalyzer
          pkgs.cargo-nextest
        ];

        RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
      };

      packages.default = rustPlatform.buildRustPackage {
        pname = "codex-proxy";
        version = "0.1.0";
        src = pkgs.lib.cleanSource ./.;
        cargoLock.lockFile = ./Cargo.lock;
        # Tests run via cargo-nextest in the devshell; the sandbox build
        # skips them (network-bound integration tests bind localhost).
        doCheck = false;
        meta.mainProgram = "codex-proxy";
      };

      checks.build = self.packages.${system}.default;
    });
}
