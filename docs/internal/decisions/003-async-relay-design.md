---
id: ADR-003
title: Async SSE Relay — Generic I/O API, Stop-After-Done, Pass-Through Scope
status: accepted
date: 2026-06-11
---

# ADR 003: Async SSE Relay — Generic I/O API, Stop-After-Done, Pass-Through Scope

*Date:* 2026-06-11
*Status:* accepted

## Context

ADR 002 decided *what* the Phase 2 relay must do: keepalive on idle (fix for
garden#803), guaranteed `[DONE]` + error frame on every exit path (fix for
garden#796), and no token-size cap. This ADR records the *shape* decisions made
while implementing it in `src/relay.rs`, and the places where the relay
deliberately does not mirror Go.

## Decision

### 1. Generic `AsyncBufRead → AsyncWrite` functions, not a `Stream` type

The relay API mirrors Go's `io.Reader → io.Writer` signature:

```rust
pub async fn rewrite_sse_stream<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    upstream: R, downstream: W, model: &str, config: &RelayConfig,
) -> Result<(), RelayError>
```

rather than returning an `impl Stream<Item = Bytes>`. Reasons:

- **Zero new deps.** `tokio` (`io-util`, `time`, `macros`) is the only addition.
  A stream-shaped API would pull in `futures-util`/`async-stream` or a manual
  state machine over timers.
- **Keepalive timing lives in one `tokio::select!`** between a cancel-safe
  `next_event()` and `sleep_until(last_write + interval)` — exactly the ADR 002
  amendment. A pull-based stream would make progress (and therefore keepalive
  timing) dependent on consumer polling.
- **Phase 4 bridging is standard:** the axum handler spawns the relay writing
  into one half of `tokio::io::duplex` and serves the other half as the
  response body (`ReaderStream` → `Body::from_stream`).

### 2. Cancel-safe SSE event reader

The keepalive `select!` drops the in-flight `next_event()` future whenever the
timer fires. `SseEventReader` therefore keeps the partial line buffer and
accumulated data lines in `self`, and reads lines with `read_until` (which
appends into the caller's buffer and is documented cancel-safe). No bytes can
be lost to a keepalive tick.

Framing matches Go's relay loop: `data:` lines accumulate and join with `\n` at
a blank-line boundary; comment lines and non-`data` fields are ignored; a
trailing event without a terminating blank line is flushed at EOF; one
trailing `\r` per line is stripped (Go's `bufio.ScanLines`). Lines grow without
a cap — ADR 002 §4 retires Go's 10MB scanner limit.

### 3. Stop after `Done` (divergence from Go)

Go's rewrite loop keeps scanning after an upstream `[DONE]` and will emit
frames *after* the sentinel. ADR 002 requires `[DONE]` to be the final frame,
so this relay returns at `TransformResult::Done`. Real upstreams close after
`[DONE]`; anything after it is protocol garbage we choose not to forward.

### 4. Typeless-JSON recovery (Go parity at the error boundary)

The Rust `EventEnvelope` requires a string `type`, so an OpenAI
`chat.completion.chunk` (no `type` field) surfaces as a transform error. The Go
transformer reads `type` out of a loose map and treats it as `""` — an unknown
event — and the Go relay then probes the raw payload for chunk pass-through.
The relay reproduces this exactly: on transform error, valid JSON *without* a
string `type` is treated as swallowed (probed for chunk pass-through, otherwise
ignored); invalid JSON, or a typed-payload failure on a recognized event,
aborts via the error-frame + `[DONE]` path.

### 5. Spec-correct multi-line frame emission (divergence from Go)

Go writes a multi-line event payload after a single `data: ` prefix, embedding
raw newlines that corrupt SSE framing (the continuation lines parse as unknown
fields downstream). `write_data_frame` instead prefixes every payload line with
`data: ` within one event, which clients rejoin with `\n` — an exact
reconstruction of the upstream event. Transformed frames are serde-encoded
JSON and never contain newlines, so this only affects pass-through of
multi-line events.

### 6. Zero keepalive interval disables keepalive

`sleep_until(last_write + Duration::ZERO)` is perpetually ready, which would
turn the `select!` loop into a ping flood that starves upstream reads. A zero
`keepalive_interval` therefore parks the keepalive branch on a pending future —
zero means "disabled", not "continuous".

### 7. Pass-through relay: keepalive yes, injection no

`pass_through_sse_stream` (Go `PassThroughSSEStream`, used by the
`/v1/responses` path) gains the same idle-deadline keepalive — SSE comments are
legal in any SSE stream. But it does **not** inject `[DONE]` at EOF or error
frames: Responses-API streams have no `[DONE]` sentinel and a fabricated
chat-completions error frame would be foreign to that protocol. Errors surface
to the caller via `RelayError` for the Phase 4 handler to log.

## Options considered

### `impl Stream<Item = Bytes>` return type

Composes directly with `Body::from_stream`. Rejected for the dep and
polling-dependence reasons in §1; the duplex bridge gives Phase 4 the same
composition with the loop fully owning its timing.

### Spawned-task + `mpsc` channel API

Decouples read and write sides. Rejected: forces an executor dependency into
the library seam, loses backpressure (unbounded buffering or an arbitrary
channel depth), and the relay is strictly sequential anyway.

### Injecting `[DONE]`/error frames in pass-through

Symmetric with the rewrite relay. Rejected: wrong protocol for Responses-API
streams (see §7).

## Risks

- **Per-frame flush.** Every frame (and keepalive) flushes the downstream
  writer. Over the Phase 4 duplex bridge a flush is just a wakeup, but a future
  buffered writer in the chain must tolerate the call frequency.
- **Keepalive granularity.** The idle deadline resets on any successful write.
  A pathological upstream emitting one tiny frame every 14.9s keeps the
  connection alive without pings — which is correct, but means keepalive
  cadence is not a fixed heartbeat.

## Related

- ADR 002 — the requirements this implements (keepalive, termination, no cap)
- `jmmaloney4/garden` #796, #803 — the production bugs fixed here
- Go source: `internal/server/transform.go:1225-1394`,
  `internal/server/server.go:547-655`

## Supersedes / Dependencies (optional)

- depends on: ADR 001, ADR 002
