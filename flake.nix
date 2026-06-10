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
    in {
      devShells.default = pkgs.mkShell {
        packages = [
          toolchain
          rustAnalyzer
          pkgs.cargo-nextest
        ];

        RUST_SRC_PATH = "${toolchain}/lib/rustlib/src/rust/library";
      };
    });
}
