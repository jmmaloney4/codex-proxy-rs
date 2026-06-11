---
id: ADR-004
title: HTTP Server Architecture, Kubernetes-Shaped Credentials, and the Go Divergence Register
status: accepted
date: 2026-06-11
---

# ADR 004: HTTP Server Architecture, Kubernetes-Shaped Credentials, and the Go Divergence Register

*Date:* 2026-06-11
*Status:* accepted

## Context

Phases 1–3 ported the pure library surface; the relay (ADR 003) made the
stream pipeline real. Phase 4 adds the axum HTTP server and a runnable
binary; Phase 5 adds self-refreshing OAuth credentials. This ADR records the
architecture and every deliberate divergence from the Go reference.

The deployment target is a 1-replica Kubernetes pod (garden homelab). That
target, not full Go parity, shapes the credential design.

## Decision

### 1. Server shape

- **axum 0.8** over bare hyper: the router/middleware/extractor layer is the
  standard choice and the relay bridge needs nothing exotic.
- `AppState` carries `Arc<dyn CredentialsFetcher>`, the shared
  `reqwest::Client`, `RelayConfig`, the upstream URL (injectable for tests),
  and the `ADMIN_API_KEY` snapshot.
- Route table mirrors Go `setupRoutes`, including the detail that the **admin
  gate protects the data plane**: clients authenticate to the proxy with
  `ADMIN_API_KEY` (Bearer or `X-API-Key`) on `/v1/chat/completions` and
  `/v1/responses`, not just `/admin/*`.

### 2. Streaming bridge

Per ADR 003's plan: the handler spawns the relay writing into one end of
`tokio::io::duplex(64KiB)`; the response body is
`Body::from_stream(ReaderStream::new(rx))`. `simplex` was tried first and
rejected: dropping a simplex `WriteHalf` never surfaces EOF to its
`ReadHalf`, so the response body hangs after the relay finishes (covered by
a regression test). Client disconnect drops the body
→ the relay's next write (a frame or a keepalive ping) fails with
`BrokenPipe` → the task exits and drops the upstream reader → reqwest cancels
the upstream request. Relay errors are logged in the spawned task; the stream
itself is already correctly terminated by the relay (ADR 002).

Upstream bytes flow in via
`BufReader::new(StreamReader::new(resp.bytes_stream().map_err(io::Error::other)))`.

### 3. Upstream client

reqwest with **rustls** (no system TLS — keeps the nix-built image free of
openssl), `http2` for ALPN, 10s connect timeout, and **no total/read
timeout** — Go parity, required for long SSE streams. Proxy-from-env is
reqwest's default. The exact Go header set (version 0.125.0, originator,
user-agent, beta features, per-request UUIDv4 `session_id` and turn
metadata) is reproduced verbatim; 401 triggers one refresh+retry.

### 4. Credentials: Kubernetes-shaped, not Go-parity (owner decision)

Only two stores exist:

- **env** (Phase 4): static token from the legacy `ANTHROPIC_API_KEY` /
  `CLAUDE_USER_ID` names; refresh is a no-op.
- **fs** (Phase 5): Go's `auth.json` format on a writable volume (PVC), with
  in-process OAuth refresh — per-request expiry check (60-min buffer),
  401-triggered refresh, 10-min background ticker, single-flight via mutex
  with a double-check. Rotated refresh tokens are persisted by atomic
  write-back (tmp + rename, 0600/0700), so the token chain survives pod
  restarts. Bootstrap via `POST /admin/credentials` (creates the file) or a
  pre-seeded volume.

**Not ported, by decision:** macOS keychain store, legacy-path migration,
`auto` mode (all macOS local-dev conveniences), and the Cloudflare KV/WASM
worker variant.

The Go optional-interface pattern (`OAuthCredentialsFetcher` reached by type
assertion) becomes default-`Err(OAuthUnsupported)` methods on the one
`CredentialsFetcher` trait (`async_trait`, `Arc<dyn>`): same capability
probing, no downcasts.

### 5. gpt-5.3-codex-spark and the WebSocket upstream: dropped (owner decision)

Go routes exactly one model over a WebSocket transport
(`upstream_websocket.go`). This is **not planned** for the Rust port: no
tokio-tungstenite, no transport selection; spark is filtered out of the
`/v1/models` dump. Requests naming spark are not special-cased — they go
over HTTP like any other model and the upstream decides.

### 6. /v1/models: embedded verbatim dump

The ~430-line Go metadata registry is not hand-ported. The response of Go's
`supportedModels()` was dumped once (spark filtered, 54 entries) into
`src/server/models.json`, embedded via `include_str!`, and served verbatim.
Model additions upstream require re-dumping — acceptable for static
metadata; the procedure lives in the PR description.

### 7. Config

clap with env fallbacks (figment is the pinned config driver but adoption is
deferred until the config surface earns it — issue #3). Legacy names
(`PORT`, `ADMIN_API_KEY`, `ANTHROPIC_API_KEY`, `CLAUDE_USER_ID`, `ENV`) are
honored for drop-in parity; new variables are `CODEX_PROXY_`-prefixed
(`CODEX_PROXY_CREDS_STORE`, `CODEX_PROXY_KEEPALIVE_SECS`, …). `ENV` selects
tracing output: dev → pretty console, else JSON (Go logger parity).

## Divergence register (vs Go)

1. **Buffered completions aggregate `tool_calls` and `usage`.** Go's
   non-streaming response drops both (its `ChatMessage` is role+content
   only), silently breaking non-streaming tool use. Additive JSON fields;
   clients that ignore them see Go's exact shape.
2. **JSON error bodies** `{"error": "..."}` where Go's `http.Error` emits
   text/plain. Status codes and message texts match.
3. **`ADMIN_API_KEY` and env credentials snapshot at startup** vs Go's
   per-request `os.Getenv` — equivalent in k8s where pod env is immutable.
4. **Spark/WebSocket dropped** (§5). **k8s-only credential modes** (§4).
5. **Graceful shutdown** on SIGTERM/SIGINT (Go has none) — required for
   clean k8s rollouts.
6. **`set-cookie` stripped from mirrored upstream headers** (Go forwards all
   headers verbatim): backend session material must not cross the proxy
   boundary. **Constant-time admin-key comparison** (Go uses `!=`).
7. **Startup credential logging redacted to presence booleans**; Go logs the
   account id and a sanitized token preview per upstream request — that log
   line was not ported.
8. **Non-streaming `/v1/responses` successes are mirrored verbatim.** Go
   pushes every 2xx through `PassThroughSSEStream`, which silently empties
   non-SSE JSON bodies; this port branches on the upstream content-type
   (the same media-type check Go uses for its SSE headers) and mirrors
   non-SSE bodies through. `content-encoding` is also stripped from
   mirrored headers (we never advertise accept-encoding; stale encodings
   would corrupt if decompression were ever enabled transitively).

## Risks

- **Models dump drift**: upstream Go model additions need a re-dump. Static
  metadata, low churn; documented procedure.
- **No request-context propagation to upstream**: Go passes the inbound
  request context so a client disconnect cancels even the initial upstream
  POST; here cancellation engages once streaming starts (axum drops the
  whole handler future on disconnect, which aborts an in-flight
  `send_with_retry` too — the gap is only theoretical).

## Related

- ADR 002/003 — relay guarantees this server exposes
- Issue #3 — figment pinned, adoption deferred
- Issue #5 — pass-through field preservation (post-parity)
- Go source: `internal/server/server.go`, `admin.go`,
  `chat_completions_buffered.go`, `client.go`, `internal/auth/`,
  `internal/credentials/`
