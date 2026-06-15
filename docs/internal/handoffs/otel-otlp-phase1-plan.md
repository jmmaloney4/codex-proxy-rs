# Implementation Plan — OTLP Trace Export (ADR-005, Phase 1)

Scope: generic OpenTelemetry spans → Tempo via the garden `alloy-llm-traces`
collector, gated on `OTEL_EXPORTER_OTLP_ENDPOINT`. No Langfuse, no message
content, no metrics. See `docs/internal/decisions/005-otel-otlp-trace-export.md`.

Work is ordered so the tree compiles and tests stay green after each step. The
feature is dark (no behavioral change) until env is set — safe to merge before
garden wiring lands.

> API note (verified June 2026): the OTel Rust stack is on **0.32** / 
> tracing-opentelemetry **0.33**, and several pre-0.30 APIs named in earlier
> drafts are gone. Corrected names are used below. Key removals:
> `new_pipeline()`/`new_exporter()`, the runtime arg to `with_batch_exporter`,
> and `global::shutdown_tracer_provider()`.

## Step 1 — Dependencies

`Cargo.toml`: replace the deferral comment (`:50`) with the pinned stack from
ADR-005 §2 (the **0.32 / 0.33** block). Confirm `opentelemetry-otlp` is
`default-features = false` with `["trace", "http-proto", "reqwest-client"]` so
**no tonic/gRPC** is pulled, and `metrics`/`logs`/blocking-client defaults are
dropped. `cargo tree -i tonic` must return nothing. `rt-tokio` on
`opentelemetry_sdk` is **not** needed for the HTTP batch path — omit it.

## Step 2 — `init_tracing` → layered Registry + provider handle

`src/config.rs:70`. Change signature `pub fn init_tracing(env: &str)` →
`-> Option<SdkTracerProvider>` (the flush handle; `None` when export is off).
The current `()` return is unused at the one call site, so this is safe.

- Keep `EnvFilter` and the dev/prod `fmt` layer exactly as-is, but build them as
  **layers on a `Registry`** rather than via `fmt().init()`.
- If `OTEL_EXPORTER_OTLP_ENDPOINT` is set:
  - Build the resource with **`Resource::builder()`** (NOT `builder_empty()` —
    only `builder()` ships `EnvResourceDetector`, which merges
    `OTEL_SERVICE_NAME` / `OTEL_RESOURCE_ATTRIBUTES`). Add `service.name`
    default `codex-proxy`, `service.version = env!("CARGO_PKG_VERSION")`,
    `deployment.environment` from the `env` arg.
  - Build the exporter:
    `opentelemetry_otlp::SpanExporter::builder().with_http().with_protocol(Protocol::HttpBinary).build()?`
    (endpoint comes from `OTEL_EXPORTER_OTLP_ENDPOINT`).
  - `SdkTracerProvider::builder().with_resource(res).with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn))).with_batch_exporter(exporter).build()`
    — **no runtime argument** to `with_batch_exporter` in 0.32; the batch
    processor runs its own background thread. `ParentBased(AlwaysOn)` (ADR §9)
    follows LiteLLM's sampling when a parent exists, samples all when root. Set
    the provider global.
  - `opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new())`.
  - Add `tracing_opentelemetry::layer().with_tracer(provider.tracer("codex-proxy"))`
    to the Registry. Return `Some(provider)`.
- Else: register only filter + fmt, return `None`.

Update the single caller `src/main.rs:15` to bind the handle:
`let otel = init_tracing(&config.env);`

## Step 3 — Flush on shutdown

`src/main.rs`, after `axum::serve(...).await?` (`:66`) returns (main is
`anyhow::Result<()>`, so code after the `.await?` runs on clean shutdown): if
`let Some(provider) = otel`, call `provider.force_flush()` then
`provider.shutdown()` (log + swallow any error). `global::shutdown_tracer_provider()`
no longer exists — you flush the held provider directly. `shutdown_signal`
(`:96`) is unchanged.

## Step 4 — Inbound context extraction + server span

`src/server/middleware.rs:18` (`log_requests`). Before creating the per-request
span:

- Build a parent `Context` via the global propagator's `extract` over an
  `opentelemetry_http::HeaderExtractor` of `req.headers()`.
- Create the request span (`info_span!("http.request", ...)`) with
  `http.request.method`, `url.path` set, and `span.set_parent(parent_cx)`.
- Instrument the downstream `next.run(req)` with that span (`.instrument(span)`),
  and record `http.response.status_code` + duration on completion (the existing
  `Instant` timing at `:27` feeds this).

Keep the existing `tracing::info!("request", ...)` line — it now lands as an
event inside the span.

## Step 5 — Upstream span (no outbound injection)

`src/upstream.rs::send_codex_request` (~`:52`). Wrap it in an `upstream.codex`
span (via `#[tracing::instrument(name = "upstream.codex", skip_all, …)]`) so the
ChatGPT request latency and the 401-refresh/retry path appear under the request
trace. Both `send_codex_request` and `send_with_retry` are `async fn`, so the
span nests under the request span automatically.

**Do NOT inject `traceparent` outbound.** The only upstream is the external
ChatGPT backend, which doesn't participate in our trace, so a wire header would
leak internal correlation IDs to a third party for no benefit — the local span
hierarchy is built by `tracing`, not by any echoed header (ADR 005 §5). Add
injection only at a traced *internal* upstream if one is ever introduced.

## Step 6 — Instrument spawned tasks (do not skip)

- `src/server/stream.rs:90` — the relay/transformer `tokio::spawn`: wrap the
  spawned future with `.instrument(tracing::Span::current())` so the server span
  stays open across the whole SSE stream and the relay duration is captured.
- Credential refresher background task — same treatment if its spans should
  correlate; otherwise it gets its own root span (acceptable).

## Step 7 — Tests

- Existing suite must pass unchanged with **no** `OTEL_*` env set (export off).
- Add one test asserting `init_tracing` returns `None` when
  `OTEL_EXPORTER_OTLP_ENDPOINT` is unset and `Some` when set (guard the global
  registry init so it doesn't conflict across tests — init once or use a probe
  on the builder, not `try_init` twice).
- Propagation unit test: feed a request with a known `traceparent`, assert the
  server span's parent carries that `trace_id`. Use a test exporter /
  `InMemorySpanExporter` rather than a live collector.

## Step 8 — Verify against a live collector (manual)

Port-forward or point at `alloy-llm-traces` HTTP `:4318`:

```
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
OTEL_SERVICE_NAME=codex-proxy cargo run
```

Send a request with a `traceparent` header, confirm a span lands in Tempo under
that `trace_id` with `service.name=codex-proxy`. Then confirm SIGTERM flushes
(no dropped tail span on shutdown).

## Step 9 — Garden wiring (separate PR, `jmmaloney4/garden`)

Two changes, both in `jmmaloney4/garden`:

1. `deploy/services/litellm/codex-proxy.ts`: inject `OTEL_EXPORTER_OTLP_ENDPOINT`,
   `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf` (not `grpc` — HTTP-only binary),
   `OTEL_SERVICE_NAME=codex-proxy`, and
   `OTEL_RESOURCE_ATTRIBUTES=deployment.environment=<env>,codex.account=<slug>`
   into each per-account Deployment's `env`.
2. **Required for nesting** (ADR-005 §11): add
   `forward_traceparent_to_llm_provider: true` to `extraLiteLLMSettings`
   (`proxy-plan.ts:701-703`). Verified: LiteLLM's `otel` callback is
   extraction-only and does NOT forward `traceparent` downstream without this
   flag; garden does not currently set it. Without it codex-proxy traces are
   valid but root (unparented).

## Sequencing

Steps 1–8 ship in one codex-proxy-rs PR (feature dark until env set). Step 9 is
a follow-up garden PR. They can land in either order.
