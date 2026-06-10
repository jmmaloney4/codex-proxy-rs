# Handoff: codex-proxy-rs PR #1 Review — SSE Transformer Prototype + Architecture for Full Rust Port

## Objective

Review PR #1 on `jmmaloney4/codex-proxy-rs` (branch `feat/sse-scaffold` → `main`) for correctness, idiomatic Rust, and architectural soundness. Then evaluate whether the current codebase, ADR decisions, and planned module decomposition are a credible foundation for replacing the full Go `codex-proxy` in production.

This is both a code review and an architecture review. The PR is the first increment of a full Go→Rust rewrite. The reviewer should assess not just what's in the diff, but whether the decisions made so far will hold up as the remaining ~5,600 lines of Go get ported.

## Scope

**In scope for this review:**
- All code in the PR: 7 commits on `feat/sse-scaffold`
- ADR 001 (typed SSE event models) and ADR 002 (keepalive, termination, structured returns)
- The planned architecture for the full Rust port (server, auth, credentials, relay)
- Whether the current module decomposition will accommodate the remaining Go subsystems without painful reorganization
- The `TransformResult` enum as the seam between transformer and relay

**Out of scope:**
- Implementing any new code — this is a review only
- Nix flake / devshell mechanics (these are stable)
- Deployment config changes (garden repo)

## Project Context

### What codex-proxy does

`codex-proxy` is a reverse proxy that sits between Codex CLI (or any OpenAI-compatible agent harness) and the ChatGPT backend at chatgpt.com. It does two things:

1. **Request transformation:** Rewrites incoming OpenAI-format chat completion requests into Codex/Responses API format, including model name normalization, reasoning effort clamping, system prompt injection, and prompt caching.
2. **SSE stream transformation:** Rewrites the upstream SSE event stream from Codex/Responses API format back into OpenAI chat-completion chunk format in real-time, including reasoning events, tool-call assembly, and usage aggregation.

The proxy currently runs as a single Go binary serving HTTP on port 9879, deployed as a 1-replica Kubernetes pod in the homelab (`garden` repo). It fronts the `codex-proxy-codex2.codex-proxy.svc.cluster.local:9879` service.

### Why the Rust rewrite

The Go implementation has production bugs documented in two garden issues:
- **garden#796** — SSE streams silently truncate mid-tool-call. The Go handler at `server.go:645-649` logs errors and returns without emitting `data: [DONE]\n\n`, so every underlying failure surfaces as an indistinguishable silent truncation.
- **garden#803** — The transformer emits zero bytes during reasoning/tool-call assembly events, allowing a ~60s idle timeout to fire and kill the connection.

Both stem from architectural gaps in the Go code: the transformer has no way to signal "handled but no output" vs "stream ended," and the server relay has no keepalive mechanism. The Rust port fixes these by design (ADR 002).

### Go source structure (the porting target)

```
jmmaloney4/codex-proxy/                      ~6,350 lines total
├── cmd/
│   ├── codex-proxy/main.go                  272 lines (CLI entrypoint, flag parsing, server startup)
│   └── claude-code-proxy-worker/main.go      31 lines (worker variant)
├── internal/
│   ├── app/app.go                            13 lines
│   ├── auth/                                302 lines (OAuth token fetch, refresh, types)
│   │   ├── fetcher.go                       221 lines
│   │   ├── oauth.go                          64 lines
│   │   └── types.go                          17 lines
│   ├── credentials/                         882 lines (keychain, Cloudflare KV, filesystem, env var)
│   │   ├── cloudflare_kv.go                 148 lines
│   │   ├── env.go                            36 lines
│   │   ├── fetcher.go                        22 lines
│   │   ├── fs.go                            133 lines
│   │   ├── keychain.go                      279 lines
│   │   └── xdg.go                            40 lines
│   ├── env/                                  44 lines (worker env config)
│   ├── logger/logger.go                      78 lines
│   └── server/                           4,706 lines (the core)
│       ├── server.go                        992 lines (HTTP handlers, routing, middleware)
│       ├── transform.go                   1,394 lines (SSE transformer, model normalization, helpers)
│       ├── models.go                        536 lines (model registry, metadata, allowed efforts)
│       ├── transform_responses.go           146 lines (non-streaming response transformation)
│       ├── transform_sse_test.go            333 lines (SSE transformer integration tests)
│       ├── transform_test.go                246 lines (unit tests for normalizeModel etc.)
│       ├── transform_responses_test.go      184 lines (response transformation tests)
│       ├── upstream_websocket.go            202 lines (WebSocket relay for Responses API)
│       ├── chat_completions_buffered.go     187 lines (buffered/non-streaming completions)
│       ├── prompts.go                       157 lines (system prompt templates, token replacement)
│       ├── admin.go                          71 lines (health/debug endpoints)
│       ├── client.go                         30 lines (HTTP client factory)
│       ├── client_workers.go                 40 lines
│       ├── types.go                          57 lines
│       ├── upstream_selector.go              19 lines
│       └── upstream_websocket_workers.go     16 lines
```

### Current Rust implementation (what's in the PR)

```
codex-proxy-rs/                             ~1,400 lines
├── Cargo.toml                               (6 deps: serde, serde_json, thiserror, regex, sha2, uuid)
├── flake.nix                                (fenix devshell, flake-utils eachDefaultSystem)
├── src/
│   ├── lib.rs                               (pub mod model; pub mod transform;)
│   ├── model.rs                             482 lines (normalize_model, normalize_reasoning_effort,
│   │                                          clamp_reasoning_effort_for_model + 54 tests)
│   └── transform/
│       ├── mod.rs                           352 lines (SSETransformer, TransformResult enum,
│       │                                    8 event handlers, event dispatch)
│       ├── upstream.rs                      ~160 lines (EventEnvelope, payload structs,
│       │                                    extract_reasoning_content cascade)
│       └── openai.rs                        ~100 lines (ChatCompletionChunk, ChunkDelta, Usage,
│                                            ToolCallDelta, FunctionDelta)
├── tests/
│   └── transform_sse.rs                     15 integration tests
└── docs/internal/decisions/
    ├── 001-typed-sse-event-models-and-loose-edge-payloads.md
    └── 002-sse-keepalive-guaranteed-termination-structured-returns.md
```

## Decisions Already Made

1. **Rust edition 2024**, fenix devshell, flake-utils `eachDefaultSystem`. Must build on `aarch64-darwin` + `x86_64-linux`.
2. **Library crate first**, no binary/server yet. The transformer and model helpers are a pure library. The async server layer comes later.
3. **Typed serde structs for stable events** (`EventEnvelope` with `#[serde(flatten)]` for the rest). "Typed core, loose edges." (ADR 001)
4. **String-based event dispatch**, NOT `#[serde(tag = "type")]` enum — needed because reasoning events require prefix matching on `response.reasoning*`. (ADR 001 revision)
5. **`TransformResult` enum** (`Emitted`/`Swallowed`/`Done`) replaces Go's `(bytes, bool)` pair. The `Swallowed` variant enables keepalive logic in the relay. (ADR 002)
6. **SSE keepalive comments** (`: ping\n\n`) emitted by the relay when `Swallowed` and idle interval elapsed. Default 15s. (ADR 002)
7. **Guaranteed `[DONE]` sentinel** on all exit paths, including error. Error events emitted before sentinel. (ADR 002)
8. **No hard token-size limit** — Rust relay won't use Go's `bufio.Scanner` 10MB cap.
9. **Out of scope for current phase:** tokio, hyper, reqwest, tracing, opentelemetry, clap, WebSocket, auth, credentials, CLI, model registry, WASM.
10. **Agent worktree convention:** All work in `.worktrees/sse-scaffold`, never in main checkout at `~/git/github.com/jmmaloney4/codex-proxy-rs/`.

## Constraints and Preferences

- **All CI disabled in yard.** Local validation required: `nix develop -c cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`.
- **Rust edition 2024** — uses let-chains, gen blocks, and other 2024 features.
- **`thiserror` v2** for error types.
- **`rstest`** for parameterized tests. `pretty_assertions` for test output. `insta` available but not yet used.
- **Prefer `&'static str` returns** for functions that return from a fixed set of strings (model IDs, effort levels). `String` only when the input is pass-through (e.g., `clamp_reasoning_effort_for_model` with an unrecognized model).
- **No large file writes** in a single tool call — break into patches.
- **Conventional commits** — `feat(scope):`, `docs(adr):`, `refactor(transform):`.
- **ADR convention** — Zeus pattern: YAML frontmatter (`id`, `title`, `status`, `date`), numbered `docs/internal/decisions/NNN-kebab-case.md`.
- **Prefer standard deps first** (serde, thiserror) — reach for alternatives only if standard actually fails.
- **ADR-level state tracking for both successes AND failures.** When reverting a complex approach to a simpler standard, write an ADR recording what failed and why.
- **Trunk-based migrations:** env var toggles, not long-lived branches.

## Relevant Files and Paths

### Rust (in PR)
- `src/lib.rs` — module root (`pub mod model; pub mod transform;`)
- `src/model.rs` — `normalize_model()`, `normalize_reasoning_effort()`, `clamp_reasoning_effort_for_model()`, 13 model constants, per-model effort maps, 54 unit tests
- `src/transform/mod.rs` — `SSETransformer` struct, `TransformResult` enum, `transform()` entry point, 8 event handlers
- `src/transform/upstream.rs` — `EventEnvelope`, payload structs (`CreatedPayload`, `OutputTextDeltaPayload`, `CompletedPayload`, `OutputItemAddedPayload`, `FunctionCallArgsDeltaPayload`), `extract_reasoning_content()` cascade
- `src/transform/openai.rs` — `ChatCompletionChunk`, `ChunkChoice`, `ChunkDelta`, `ToolCallDelta`, `FunctionDelta`, `Usage`
- `tests/transform_sse.rs` — 15 integration tests covering all Go `transform_test.go` cases
- `docs/internal/decisions/001-typed-sse-event-models-and-loose-edge-payloads.md` — ADR 001
- `docs/internal/decisions/002-sse-keepalive-guaranteed-termination-structured-returns.md` — ADR 002
- `Cargo.toml` — edition 2024, 6 deps
- `flake.nix` — fenix devshell

### Go (reference, not in PR)
- `internal/server/transform.go` — 1,394 lines. The primary porting target.
- `internal/server/transform_test.go` — 246 lines. Test cases already ported.
- `internal/server/transform_sse_test.go` — 333 lines. SSE integration tests partially ported.
- `internal/server/models.go` — 536 lines. Model registry with metadata.
- `internal/server/server.go` — 992 lines. HTTP handlers, routing, the relay loop with the silent-truncation bug.
- `internal/server/transform_responses.go` — 146 lines. Non-streaming response transformation.
- `internal/server/prompts.go` — 157 lines. System prompt templates, `replaceNames` token replacement.
- `internal/server/chat_completions_buffered.go` — 187 lines. Buffered completions.
- `internal/server/upstream_websocket.go` — 202 lines. WebSocket relay.
- `internal/auth/` — 302 lines. OAuth token management.
- `internal/credentials/` — 882 lines. Keychain, Cloudflare KV, filesystem credential storage.
- `cmd/codex-proxy/main.go` — 272 lines. CLI entrypoint.

### Worktree
- `/Users/jack/git/github.com/jmmaloney4/codex-proxy-rs/.worktrees/sse-scaffold` — where all edits happen
- `/Users/jack/git/github.com/jmmaloney4/codex-proxy-rs/` — main checkout, reserved for Jack

## Relevant URLs and References

- **PR:** https://github.com/jmmaloney4/codex-proxy-rs/pull/1
- **Repo:** https://github.com/jmmaloney4/codex-proxy-rs
- **Go source:** https://github.com/jmmaloney4/codex-proxy (private)
- **garden#796:** https://github.com/jmmaloney4/garden/issues/796 — silent stream truncation
- **garden#803:** https://github.com/jmmaloney4/garden/issues/803 — idle timeout from slow completions
- **Deployment config:** `jmmaloney4/garden` repo, `deploy/services/litellm/codex-proxy.ts`

## Implementation Notes

### What's been ported so far

All test-verified code from Go `transform.go` and `transform_test.go`:

| Go function | Rust location | Tests |
|---|---|---|
| `SSETransformer` struct + `Transform` | `src/transform/mod.rs` | 15 integration |
| `NewSSETransformer` | `SSETransformer::new()` | covered |
| `handleCreated` | `handle_created()` | covered |
| `handleOutputTextDelta` | `handle_text_delta()` | covered |
| `handleReasoning` | `handle_reasoning()` | covered |
| `handleCompleted` | `handle_completed()` | covered |
| `handleOutputItemAdded` | `handle_output_item_added()` | covered |
| `handleFunctionCallArgsDelta` | `handle_function_call_args_delta()` | covered |
| `extractReasoningContent` cascade | `EventEnvelope::extract_reasoning_content()` | covered |
| `normalizeModel` | `normalize_model()` | 27 unit |
| `normalizeReasoningEffort` | `normalize_reasoning_effort()` | 9 unit |
| `clampReasoningEffortForModel` | `clamp_reasoning_effort_for_model()` | 18 unit |

### What remains to be ported (ordered by dependency)

**Phase 1 — Pure helpers (no new deps):**
- `replaceNames` — token name replacement in prompts (regex-based)
- `resolveRequestModel` — extract model from request JSON
- `resolveReasoningEffort` / `resolveReasoningSummary` / `buildReasoningSettings` — request preprocessing
- `derivePromptCacheKey` — SHA-256 → UUID v5 cache key derivation
- `TransformSSELine` — top-level entry point wrapping `SSETransformer`

**Phase 2 — Server relay (needs tokio + hyper/axum):**
- SSE relay loop consuming `TransformResult` — the core fix for garden#796/#803
- Keepalive timer on `Swallowed` variant
- Guaranteed `[DONE]` + error event on all exit paths
- Streaming line reader (no bufio.Scanner 10MB limit)
- `RewriteSSEStreamWithCallback` equivalent
- `PassThroughSSEStream` equivalent

**Phase 3 — Request transformation (needs server layer):**
- `TransformRequest` — rewrite incoming OpenAI requests to Codex/Responses API
- `TransformResponses` — non-streaming response transformation
- `buildCodexRequest` / `buildResponsesRequest` — request construction
- System prompt injection, tool mapping

**Phase 4 — HTTP server (needs auth):**
- `server.go` handlers — `/v1/chat/completions`, `/v1/responses`, health, admin
- Route matching, middleware, CORS
- `client.go` — upstream HTTP client with HTTP/2, no timeout on SSE streams

**Phase 5 — Auth + credentials:**
- OAuth token fetch/refresh (`auth/fetcher.go`)
- Credential storage: keychain (macOS Security framework), Cloudflare KV, filesystem, env vars
- `internal/credentials/` → Rust equivalents

**Phase 6 — CLI + model registry:**
- `clap`-based CLI with `cmd/codex-proxy/main.go` equivalent
- Model registry from `models.go` (536 lines of model metadata)
- WebSocket relay from `upstream_websocket.go`

### Key architectural questions for the reviewer

1. **Is `src/transform/mod.rs` the right module structure?** Currently a single `mod.rs` with private submodules `openai` and `upstream`. As handlers grow (and more get ported from `transform.go`), should this split into separate handler files or stay flat?

2. **Is `TransformResult` the right abstraction boundary?** The relay will `match` on it. Should `Emitted` carry a `Vec<Vec<u8>>` (list of SSE frames) instead of `Vec<u8>` (newline-joined frames)? The current approach joins with `\n` which means the relay can't flush mid-frame.

3. **Model registry location.** `src/model.rs` currently has pure helpers + constants. The Go `models.go` (536 lines) has a full registry with metadata. Should the registry be a separate module or grow into `model.rs`?

4. **Error handling strategy.** `TransformError` has two variants (`InvalidJson`, `MarshalError`). The Go code also has network errors and auth errors. Should the error type hierarchy be designed now or deferred to the server phase?

5. **`serde_json::Value` at the envelope boundary.** `EventEnvelope.extra` is `Value` — every handler re-deserializes from it. Is this acceptable for the full port, or should we invest in a more specialized extraction pattern?

6. **Server framework choice.** The Go server uses stdlib `net/http`. The Rust equivalent would be `axum` or `hyper`. Any preference or constraint?

## Proposed Execution Plan

1. **Review the PR diff** — all 7 commits, focusing on the transformer logic and model helpers
2. **Read both ADRs** — verify the decisions are well-grounded and the implementation matches
3. **Cross-reference with Go source** — spot-check that ported logic matches Go behavior
4. **Evaluate module decomposition** — will the current structure accommodate phases 2-6 without painful reorganization?
5. **Assess test coverage** — are the 69 tests sufficient for what's been ported? Are there Go test cases we missed?
6. **Check for idiomatic Rust** — any Go-isms that should be restructured for Rust?
7. **Evaluate the `TransformResult` design** — is this the right seam between transformer and relay?
8. **Provide architecture feedback** — any concerns about the planned phase ordering or module boundaries?

## Validation and Testing

The reviewer should be able to:

```bash
cd /Users/jack/git/github.com/jmmaloney4/codex-proxy-rs/.worktrees/sse-scaffold
nix develop -c cargo fmt
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo nextest run
# Expected: 69 tests passed, 0 failed
```

Cross-reference tests against Go source:
```bash
# Go test cases are in:
# internal/server/transform_test.go      (normalizeModel, effort clamping)
# internal/server/transform_sse_test.go  (SSE transformer integration)
```

## Open Questions / Uncertainties

- **Server framework not chosen.** axum vs hyper vs actix — deferred to phase 2. Reviewer may have opinions.
- **Credential storage in Rust.** The Go code uses macOS Keychain via `keychain` package. Rust equivalent would be `keyring` crate. Not yet evaluated.
- **WebSocket relay.** The Go code has `upstream_websocket.go` (202 lines) for Responses API WebSocket support. Whether the Rust port needs this is unclear — the SSE path may be sufficient for the homelab use case.
- **`tool_name_by_item_id` is populated but never read.** In both Go and Rust, the transformer stores tool names by item ID but the SSE output never uses them (tool names go into the initial `response.output_item.added` chunk, not subsequent arg deltas). May be dead state.
- **`regex` dependency is listed but unused.** It was included for future `replaceNames` porting but is currently an unused dep.

## Copyable Tasking for Next Agent

You are reviewing PR #1 on `jmmaloney4/codex-proxy-rs` (branch `feat/sse-scaffold`, 7 commits targeting `main`). This is both a code review and an architecture review for a Go→Rust rewrite of `codex-proxy`.

**What to do:**

1. Read the full diff: `git diff main..feat/sse-scaffold` in the worktree at `/Users/jack/git/github.com/jmmaloney4/codex-proxy-rs/.worktrees/sse-scaffold`
2. Read both ADRs in `docs/internal/decisions/`
3. Cross-reference the Rust `src/transform/mod.rs` against the Go source at `/Users/jack/git/github.com/jmmaloney4/codex-proxy/internal/server/transform.go` — verify the ported logic is faithful
4. Cross-reference `src/model.rs` against Go `transform.go` lines 294-456 and `models.go`
5. Verify test coverage: compare Rust tests in `tests/transform_sse.rs` and `src/model.rs::tests` against Go tests in `internal/server/transform_test.go` and `internal/server/transform_sse_test.go`
6. Run the validation commands (`nix develop -c cargo fmt && cargo clippy --all-targets -- -D warnings && cargo nextest run`)
7. Evaluate the `TransformResult` enum design — is this the right seam between transformer and future relay?
8. Assess whether the current module structure (`src/lib.rs` → `model.rs` + `transform/`) will accommodate the remaining Go subsystems without painful reorganization
9. Provide feedback on:
   - Correctness of ported logic
   - Idiomatic Rust quality (any Go-isms to restructure?)
   - ADR decisions (are they well-grounded? any gaps?)
   - Module decomposition for the full port
   - Test coverage gaps
   - Any concerns about the planned phase ordering
10. If you find bugs, style issues, or architectural concerns, flag them clearly with suggested fixes

The Go source repo is at `/Users/jack/git/github.com/jmmaloney4/codex-proxy/`. The two production bugs this rewrite fixes are documented at `jmmaloney4/garden` issues #796 and #803.
