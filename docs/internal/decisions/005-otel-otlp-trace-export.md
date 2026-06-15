---
id: ADR-005
title: OpenTelemetry OTLP Trace Export — Tempo Distributed Tracing Behind LiteLLM
status: accepted
date: 2026-06-14
---

# ADR 005: OpenTelemetry OTLP Trace Export — Tempo Distributed Tracing Behind LiteLLM

*Date:* 2026-06-14
*Status:* accepted

## Context

ADR 004 stood up the axum server with `tracing`/`tracing-subscriber` for
structured logs and deferred OpenTelemetry to "later" (`Cargo.toml:50`). This
ADR makes it real, scoped to **distributed tracing export over OTLP**.

The deployment topology decides the design. codex-proxy is **not** a peer of
the garden LiteLLM gateway — it is an *upstream backend* of it. Per
`garden/deploy/services/litellm/codex-proxy.ts`, each backend account gets its
own Deployment + Service in the `codex-proxy` namespace, and LiteLLM calls them
in-cluster:

```
client → litellm :4000 → codex-proxy-<slug>.codex-proxy.svc.cluster.local:9879/v1 → chatgpt.com/backend-api/codex
```

codex-proxy is wired as the `api_base` for the personal `coding`/`smart`/`cheap`
model tiers (`garden/.../litellm/index.ts:181-191`, `proxy-plan.ts:317-321`).
It is reachable by **exactly one path**: a ClusterIP Service on `:9879`, called
only by LiteLLM (shared `ADMIN_API_KEY` Bearer). There is no Ingress,
HTTPRoute, tailscale-serve, or direct Codex-client route to it — so every
inbound request originates at LiteLLM, and joining LiteLLM's trace is the whole
game.

Garden's tracing architecture (garden ADRs 084 → 093) settled three rules this
ADR inherits rather than re-litigates:

1. **W3C `trace_id` is the single correlation spine.** Every hop propagates
   `traceparent`.
2. **Tempo receives generic distributed spans** via the dedicated
   `alloy-llm-traces` collector
   (`alloy-llm-traces.observability.svc.cluster.local`, OTLP/HTTP `:4318`,
   gRPC `:4317`; off-cluster via tailnet HTTPS `alloy-llm-traces.<tailnet>`,
   HTTP/protobuf only).
3. **Langfuse receives content-rich generation spans via per-source *native*
   integrations, not generic OTel** (generic GenAI spans render null-content in
   Langfuse). LiteLLM already does this with its `langfuse_otel` callback.

Because LiteLLM sits *in front* of codex-proxy, **LiteLLM already captures the
generation** (input/output/model/token usage/cost) for the call it makes to
codex-proxy. codex-proxy emitting its own Langfuse generation would be
duplicative and re-introduce the double-count hazard garden ADR 084 warns about.
So **Langfuse export is explicitly out of scope** (§ Non-goals). codex-proxy's
contribution is the *internal* span breakdown in Tempo — request transform,
credential refresh / 401 retry, upstream ChatGPT latency, SSE relay duration —
nested under LiteLLM's trace by shared `trace_id`. Complementary, not redundant.

## Decision

### 1. Generic OTLP spans → Tempo, gated on configuration

Export OpenTelemetry trace spans to the garden `alloy-llm-traces` collector via
**OTLP/HTTP (`http/protobuf`)**, reusing the existing `reqwest` client. gRPC
(tonic) is rejected: it duplicates a transport we already carry and the
collector exposes an HTTP receiver. Export is **enabled only when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set**; unset → ADR-004 behavior verbatim (fmt
logs only, zero new runtime cost). Local dev and tests stay quiet by default.

### 2. Dependencies (OTLP/HTTP over reqwest, no gRPC)

```toml
opentelemetry                      = "0.32"
opentelemetry_sdk                  = "0.32"   # rt-tokio NOT required for the HTTP batch path (see below)
opentelemetry-otlp                 = { version = "0.32", default-features = false, features = ["trace", "http-proto", "reqwest-client"] }
opentelemetry-http                 = "0.32"   # HeaderExtractor / HeaderInjector over http 1.x HeaderMap
tracing-opentelemetry              = "0.33"   # numbering leads the otel crates by one; 0.33 targets otel 0.32
```

(`opentelemetry-semantic-conventions` was evaluated and dropped: the handful of
attribute keys used — `http.request.method`, `service.version`,
`deployment.environment` — are stable OTel spec strings, not worth a dependency
whose constant module paths churn across minors.)

These crates break APIs across **minor** releases, so the minor must stay
locked. The caret ranges above do exactly that (`^0.32` resolves `<0.33`), and
the committed `Cargo.lock` — consumed by the nix build via
`cargoLock.lockFile` — freezes the exact resolved set (`0.32.0` / `0.32.1` /
`0.33.0`) for reproducibility. A `cargo update` therefore stays within the minor
and cannot silently desync the matrix; only a deliberate manifest edit crosses a
minor. (Caret, not `=`-pins: consistent with every other dep in this manifest,
and `=`-pinning would fight the resolver — `opentelemetry_sdk` already resolves
to `0.32.1` while its siblings are `0.32.0`.) The compatibility rule that
matters: `tracing-opentelemetry` is versioned one minor *ahead* of the
`opentelemetry*` crates it targets (0.33 → otel 0.32). `default-features = false`
+ the `["trace", "http-proto", "reqwest-client"]` trio fully excludes tonic/gRPC
(verified: `grpc-tonic` is the only gRPC gate and is not selected) and drops the
default `metrics`/`logs`/blocking-client pulls.

### 3. Subscriber: layered Registry, logs and traces coexist

`init_tracing` (`src/config.rs:70`) is restructured from `fmt().init()` to a
`Registry` with composed layers:

```
Registry
  ::with(EnvFilter)                          // unchanged, RUST_LOG honored
  ::with(fmt_layer)                          // unchanged stderr logs (pretty|json by ENV)
  ::with(OpenTelemetryLayer::new(tracer))    // added only when endpoint configured
```

The existing `tracing::info!/warn!/error!` call sites become **span events**
for free — no log call is rewritten.

### 4. Resource identity

`service.name = codex-proxy`, `service.version = CARGO_PKG_VERSION`,
`deployment.environment` from `ENV`. Each per-account Deployment is
disambiguated by a `codex.account=<slug>` resource attribute injected from
garden via `OTEL_RESOURCE_ATTRIBUTES` (not a new code-level config knob).

The Resource **must** be built with `Resource::builder()` (not
`builder_empty()`): only the former ships the `EnvResourceDetector` that merges
`OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES`. Build it the wrong way and
garden's injected env is silently dropped — the single most likely "why is
every span unlabeled" bug in this design.

### 5. Context propagation — join LiteLLM's trace, don't orphan it

- **Inbound (extract):** install a global `TraceContextPropagator`; in request
  middleware (extend `log_requests`, `src/server/middleware.rs:18`) build a
  parent `Context` by extracting over a `HeaderExtractor` of the request
  headers, then `set_parent` on the server span. *This is the line that nests
  codex-proxy under LiteLLM instead of starting a root trace.*
- **Outbound: deliberately NOT injected.** The only upstream is the external
  ChatGPT backend, which does not participate in our trace. Injecting
  `traceparent` there would leak internal correlation IDs to a third party with
  no benefit — the local `upstream.codex` span already nests correctly via
  `tracing`, independent of any wire header. (An earlier draft injected "for a
  future traced upstream"; that hypothetical doesn't justify a standing leak —
  add injection at that internal hop if/when it exists.)

### 6. Span shape

- **Server span** per request: `POST /v1/chat/completions`, attrs
  `http.request.method`, `url.path`, `http.response.status_code`, duration.
- **Upstream span** (`upstream.codex`) around the ChatGPT call; 401-refresh +
  retry recorded as span events. Local span only — no `traceparent` on the wire
  (see §5 outbound).
- **Metadata only — no message content** on any span by default (see
  Non-goals). `gen_ai.request.model` and `stream` are safe to attach; request
  bodies and completions are not.

### 7. Async task context (the non-obvious bug surface)

The SSE relay + transformer (`src/server/stream.rs:90`) and the credential
refresher run in detached `tokio::spawn`s. They must be wrapped with
`.instrument(Span::current())` (or handed an explicit span). Otherwise the
server span closes the instant the handler returns — before the stream
finishes — and the relay duration, the part of the trace codex-proxy uniquely
contributes, is lost.

### 8. Flush on shutdown

`BatchSpanProcessor` buffers spans off the request path. `shutdown_signal`
(`src/main.rs:96`) currently only logs; it must trigger
`tracer_provider.shutdown()` after `axum::serve` returns (the provider handle is
threaded through `main`). Without this, every pod rollout drops in-flight spans.

### 9. Sampler — `ParentBased(AlwaysOn)`

The provider uses `Sampler::ParentBased(AlwaysOn)` (set explicitly, though it is
the SDK default). For a proxy sitting *inside* another service's trace this is
the only correct choice: when LiteLLM propagates a sampled parent, codex-proxy
**follows the parent's decision** (no orphaned half-traces); when codex-proxy is
the root (no inbound `traceparent`, e.g. garden flag not yet set), it samples
everything. Head-sampling/ratio control belongs upstream at LiteLLM or in the
collector, not here — codex-proxy is too low in the stack to make that call.

### 10. Config / env (OTel-standard, garden-injectable)

| Var | Value (in-cluster) |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `http://alloy-llm-traces.observability.svc.cluster.local:4318` |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `http/protobuf` |
| `OTEL_SERVICE_NAME` | `codex-proxy` |
| `OTEL_RESOURCE_ATTRIBUTES` | `deployment.environment=prod,codex.account=<slug>` |

Standard `OTEL_*` names are honored directly so garden injects them the same way
it does for LiteLLM — no `CODEX_PROXY_`-prefixed alias is added (divergence from
ADR-004's new-var convention; OTel's own env contract wins here).
`OTEL_EXPORTER_OTLP_PROTOCOL` must be `http/protobuf`, **not** `grpc`: in the
Rust SDK the transport is chosen at compile time by `.with_http()`, and gRPC
isn't compiled in — a `grpc` value would be silently ineffective.

### 11. Companion change in garden (required for nesting — not optional)

Validation confirmed LiteLLM's `otel` callback only *extracts* an inbound
`traceparent` for its own spans; it does **not** forward `traceparent` to the
backend HTTP call unless `litellm_settings.forward_traceparent_to_llm_provider`
is `true`, and garden does **not** currently set it. So this design nests under
LiteLLM only if garden adds, to `extraLiteLLMSettings`
(`garden/.../litellm/proxy-plan.ts:701-703`):

```yaml
litellm_settings:
  forward_traceparent_to_llm_provider: true
```

codex-proxy is precisely the "self-hosted LLM" case this flag exists for. The
flag covers the chat-completions path (the only path codex-proxy serves).
Without it, codex-proxy still produces valid traces — just rooted at codex-proxy
rather than nested under the caller. The codex-proxy code change and this garden
change are independent and can land in either order; nesting requires both.

## Non-goals (by decision)

- **No Langfuse export from codex-proxy.** LiteLLM's `langfuse_otel` callback
  already captures the generation for the upstream call it makes here; a second
  emission double-counts tokens/cost (garden ADR 084). If codex-proxy is ever
  fronted by something *other* than LiteLLM, revisit as a separate ADR emitting
  Langfuse-native attributes direct to `/api/public/otel` with Basic auth.
- **No message-content / prompt capture on spans.** Matches the redaction
  default (sector7 design 026: `turn_off_message_logging: true`). Tempo gets
  timing + metadata, never bodies.
- **No metrics, no logs-over-OTLP.** Spans only. Logs stay on stderr (ADR 004).

## Risks

- **LiteLLM `traceparent` forwarding — confirmed off by default, addressed in
  §10.** Not a residual risk; it is a known prerequisite. The only live risk is
  forgetting the garden flag, which degrades gracefully to root traces (not a
  failure). Re-confirm the flag still exists if garden bumps the LiteLLM image
  far past the `main-stable` line current at writing.
- **Crate-churn / version skew.** The Rust OTel stack breaks across minors;
  pinned versions + a `cargo update` review gate this. `tracing-opentelemetry`
  must match the `opentelemetry` minor it targets.
- **Garden deployment wiring is a prerequisite.** `codex-proxy.ts` must inject
  the `OTEL_*` env; without it the feature is dormant (acceptable — gated).
- **Batch backpressure on collector outage.** If `alloy-llm-traces` is down the
  batch processor drops on overflow; it must never block the request path
  (export is fire-and-forget, request success is independent of export success).

## Divergence register (vs ADR-004 conventions)

1. **Standard `OTEL_*` env names, not `CODEX_PROXY_`-prefixed** (§10) — interop
   with the OTel ecosystem and garden's existing LiteLLM wiring outweighs local
   naming consistency.
2. **`init_tracing` returns a provider handle** (was `()`), so `main` can flush
   it on shutdown (§8). Signature change rippling into `src/main.rs:15`.

## Related

- ADR 004 — server + `init_tracing` this refactors; `Cargo.toml:50` deferral
- garden ADR 084 — OTLP fan-out, W3C `trace_id` spine, double-count warning
- garden ADR 093 — per-source native Langfuse integration (why §Non-goals)
- `garden/deploy/services/litellm/codex-proxy.ts` — deployment to wire `OTEL_*`
- `garden/deploy/services/litellm/proxy-plan.ts:701-703` —
  `forward_traceparent_to_llm_provider` (§10)
- LiteLLM `forward_traceparent_to_llm_provider` (docs.litellm.ai/docs/proxy/logging);
  extraction-only `otel` callback (BerriAI/litellm `integrations/opentelemetry.py`)
- OTel Rust 0.32 / tracing-opentelemetry 0.33 API (docs.rs)
- Implementation plan: `docs/internal/handoffs/otel-otlp-phase1-plan.md`
