---
id: ADR-002
title: SSE Keepalive, Guaranteed Stream Termination, and Structured Transform Returns
status: accepted
date: 2026-06-10
---

# ADR 002: SSE Keepalive, Guaranteed Stream Termination, and Structured Transform Returns

*Date:* 2026-06-10
*Status:* accepted

## Context

The Go `codex-proxy` has two related production failures documented in
`jmmaloney4/garden` issues #796 and #803:

1. **Silent stream truncation.** When the SSE rewrite loop encounters any error
   (network failure, scanner overflow, pod death), the Go handler at
   `server.go:645-649` logs the error and returns — but does not emit `data:
   [DONE]\n\n`. The response was already `200 OK` with `text/event-stream`
   headers. Downstream sees an abrupt close with no terminator and interprets it
   as a dropped connection. Every underlying failure mode surfaces identically.

2. **Idle-gap timeout.** The Go `SSETransformer` emits zero bytes downstream for
   several upstream event types: `response.function_call_arguments.done`,
   `response.output_item.done`, reasoning events with `output_index > 0`, and
   all unknown events. During a long reasoning phase or tool-call assembly, the
   upstream is active but the transformer produces nothing. If anything in the
   chain (litellm, an ingress controller, tailscale-serve, or the Codex client)
   has an idle/read timeout, it fires at ~60s and kills the connection — which
   then triggers the silent truncation from #1.

3. **10MB scanner limit.** The Go `bufio.Scanner` in `RewriteSSEStreamWithCallback`
   caps single SSE tokens at 10MB (`transform.go:1227`). A tool-call argument
   embedding a large file can exceed this, producing a `token too long` error
   that triggers the silent truncation from #1.

All three stem from a single architectural gap: the transformer has no way to
signal "I handled this event but produced no output," and the server relay has
no mechanism to keep the connection alive during output gaps or to guarantee
clean termination.

Our Rust `SSETransformer::transform()` currently returns `(Vec<u8>, bool)` where
the `Vec<u8>` is empty both for "nothing to emit" and for "I handled this event
but produced no output." This conflates two semantically different states.

## Decision

### 1. Structured return type for `transform()`

Replace `(Vec<u8>, bool)` with a `TransformResult` enum:

```rust
pub enum TransformResult {
    /// Bytes to emit downstream (one or more SSE data frames).
    Emitted(Vec<u8>),
    /// Upstream event was handled but produced no output.
    /// The relay layer may emit a keepalive.
    Swallowed,
    /// Stream is complete. Relay must emit `data: [DONE]\n\n`.
    Done,
}
```

This gives the relay layer enough information to distinguish "nothing happened"
from "the stream ended" from "the transformer chose not to emit."

### 2. SSE keepalive during idle gaps

When the relay receives `TransformResult::Swallowed`, it tracks time since the
last byte emitted. If a configurable interval (default 15s) has elapsed, the
relay emits an SSE comment line (`: ping\n\n`). SSE comment lines are ignored by
spec-compliant clients but reset idle timers on every proxy, load balancer, and
gateway in the chain.

This directly addresses garden#803: the upstream is still flowing (sending
reasoning events, tool-call assembly events), so keepalives will fire
regularly, preventing any idle timeout from triggering.

### 3. Guaranteed `[DONE]` and error event

The relay loop guarantees `data: [DONE]\n\n` is always the last frame emitted,
even on error. On error, the relay emits a structured error event before the
sentinel:

```
data: {"error":"stream error: <message>"}

data: [DONE]

```

This directly addresses garden#796: clients always get a deterministic stream
terminator and can distinguish clean completion from error truncation.

### 4. No hard token-size limit

The Rust relay will use a streaming line reader (not Go's `bufio.Scanner` with a
fixed 10MB cap). Individual SSE frames are read line-by-line; large payloads
are passed through without a size gate. This eliminates the `token too long`
failure mode entirely.

## Options considered

### Option A: Keepalive at the transformer level

Emit keepalive bytes from inside `transform()` on no-op events.

Rejected because: the transformer should be a pure state machine with no I/O or
timing concerns. Keepalive timing is a relay-layer responsibility. The
structured return type is the clean seam between the two.

### Option B: Timer-based keepalive in relay (independent of transformer)

Spawn a background timer that emits `: ping\n\n` every N seconds regardless of
upstream activity.

Rejected because: this emits keepalives even during active streaming, adding
unnecessary bytes. It also doesn't account for backpressure — the relay should
only keepalive when it's actually idle. The `Swallowed` signal from the
transformer is the right trigger.

### Option C: Fix the Go implementation instead

Patch `server.go:645-649` to emit `[DONE]` on error, add keepalive logic to the
Go relay loop.

Rejected because: the whole point of `codex-proxy-rs` is to replace the Go
implementation. The Rust port is the vehicle for these fixes. Patching Go is
maintenance on a codebase being replaced.

## Implementation plan

1. **Define `TransformResult` enum** in `src/transform/mod.rs`.
2. **Update `transform()`** to return `TransformResult` instead of
   `Result<(Vec<u8>, bool), TransformError>`. Map:
   - Empty output, not done → `Swallowed`
   - Non-empty output, not done → `Emitted(bytes)`
   - `[DONE]` marker → `Done`
3. **Add `KeepaliveConfig`** struct with `interval` field (default 15s).
4. **Update tests** to match the new return type (all 15 existing tests).
5. **Add relay-layer tests** for:
   - `Swallowed` triggers keepalive after interval
   - `Done` always emits `[DONE]` sentinel
   - Error path emits error event + `[DONE]`
   - Active streaming suppresses keepalive

## Risks

### Amendment: Keepalive trigger is timer-based, not purely Swallowed-triggered

The original decision described keepalive as triggered on `Swallowed` events
from the transformer. However, when the upstream goes completely silent (e.g.
a long-running model with no SSE events at all), there are no events to
transform and thus no `Swallowed` signals. The keepalive mechanism must
therefore use an **idle deadline on last-write** — a timer that fires
`N` seconds after the last byte was written downstream, independent of
upstream event arrival.

In the Phase 2 relay, this is implemented via a `tokio::select!` branch with
a read-with-deadline on the upstream SSE stream. If no event arrives within
the keepalive interval after the last write, the relay emits `: ping\n\n`
and resets the deadline. This handles both cases:

- **Active upstream, no output:** `Swallowed` events keep the relay loop
  spinning; the idle deadline resets on each keepalive emission.
- **Silent upstream:** The read-with-deadline fires, emitting a keepalive
  even with zero upstream events.

This amendment does not change the `TransformResult` API or the transformer's
responsibilities — it only clarifies the relay-layer implementation strategy.

- **Return-type change is a breaking API change.** Since `transform()` is the
  only public entry point, all callers (currently just tests) must update. This
  is fine — there are no external consumers yet.

- **Keepalive interval tuning.** 15s is conservative. Some deployments may need
  shorter intervals (e.g., 5s behind aggressive load balancers). The
  configurable interval handles this.

- **Error event format.** Emitting `{"error":"..."}` as an SSE data frame is not
  standard OpenAI behavior. Clients may not expect it. But the alternative is
  silent truncation, which is strictly worse. The error event is additive — it
  doesn't break clients that ignore unexpected data frames.

## Related

- `jmmaloney4/garden` #796 — silent stream truncation
- `jmmaloney4/garden` #803 — idle timeout from 45-63s completions
- ADR 001 — typed SSE event models (the transformer being fixed)
- Go source: `internal/server/server.go:645-649` (silent return on error)
- Go source: `internal/server/transform.go:1225-1332` (RewriteSSEStreamWithCallback)

## Supersedes / Dependencies (optional)

- depends on: ADR 001 (typed models, already implemented)
- fixes: `jmmaloney4/garden` #796, `jmmaloney4/garden` #803
