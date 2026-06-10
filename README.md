# codex-proxy-rs

Rust scaffold for a future port of `jmmaloney4/codex-proxy`, starting with the `SSETransformer` test harness.

## Scope of this phase

- Normal native compilation only
- No WASM / Cloudflare Workers support
- No HTTP server, OAuth, credentials, upstream client, WebSocket, OTel, CLI, or model registry yet
- Just enough transformer code and tests to prototype the Codex SSE → OpenAI chunk translation

## Provenance

The source of truth for behavior is the Go implementation in `jmmaloney4/codex-proxy`, especially:

- `internal/server/transform.go`
- `internal/server/transform_test.go`
- `internal/server/transform_sse_test.go`

The goal here is to port those tests incrementally and use them as the correctness oracle.

## Current scaffold

- `src/transform.rs`: stateful `SSETransformer` prototype
- `tests/transform_sse.rs`: first ported SSE test cases
- `flake.nix`: fenix-backed Rust devshell
- `.envrc`: direnv flake integration

## Deferred next-phase work

- Remaining SSE event arms: reasoning, tool call start, tool arg deltas, tool finish tracking
- Pure helper parity: model normalization, prompt-cache key derivation, reasoning text extraction
- Stream driver parity: SSE block rewriting and `[DONE]` termination behavior
- Full service port: axum/reqwest/auth/config/logging/deployment
