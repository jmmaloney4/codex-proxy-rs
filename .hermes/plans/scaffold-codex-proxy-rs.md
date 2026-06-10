# codex-proxy-rs Scaffold Implementation Plan

> **For Hermes:** Execute this plan directly in the agent worktree. Keep scope to scaffold + prototype harness only.

**Goal:** Create a private Rust library repo with a fenix-backed Nix devshell, a compiling `SSETransformer` prototype stub, and a passing Rust test that proves the Go SSE transformer tests can be ported incrementally.

**Architecture:** Use a small library crate with `src/lib.rs` and `src/transform.rs`. Keep the transformer synchronous and stateful, mirroring the Go shape closely enough to port tests without prematurely designing the later async stream layer.

**Tech Stack:** Rust 2024, Cargo, serde/serde_json, thiserror, uuid, sha2, regex, rstest, pretty_assertions, Nix flakes, fenix, flake-utils.

---

## Tasks

1. **Repository scaffold** — create the private GitHub repo, seed `main`, create agent worktree `./.worktrees/sse-scaffold`.
2. **Tooling files** — write `Cargo.toml`, `flake.nix`, `.envrc`, `.gitignore`, `README.md` and generate `flake.lock`.
3. **Prototype module** — implement `src/lib.rs` and `src/transform.rs` with the `SSETransformer` fields, `new()`, and prototype `transform()` handling `response.created`, `response.output_text.delta`, `response.completed`, plus `[DONE]`.
4. **Test harness** — port representative Go SSE tests into `tests/transform_sse.rs`, comparing parsed JSON values instead of raw strings.
5. **Validation** — run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo nextest run`; confirm `nix develop` shell resolves fenix Rust tools.
6. **Git handoff** — commit on `feat/sse-scaffold`, push, and leave a README note listing deferred event arms and later-phase deps.

## Risks

- `cargo nextest` may not be available outside the flake shell; include it in the devshell and fall back to plain `cargo test` only if necessary during bootstrap.
- Empty newly created GitHub repos need a seeded `main` before `git worktree add` works.
- Fenix `rust-analyzer` is packaged separately from `stable.withComponents`; wire it explicitly.

## Verification

- `Cargo.toml` resolves and compiles on `aarch64-darwin`.
- `flake.nix` evaluates for at least the local system and includes `cargo`, `rustc`, `rustfmt`, `clippy`, `rust-src`, `rust-analyzer`, and `cargo-nextest`.
- Rust test output proves the Go test-porting pattern end-to-end for the first SSE cases.
- README states provenance and explicitly excludes WASM/full server work from this phase.
