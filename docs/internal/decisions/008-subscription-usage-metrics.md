---
id: ADR-008
title: Subscription-Usage Observability — Prometheus Gauge to Mimir + Span Attributes to Tempo
status: accepted
date: 2026-06-18
---

# ADR 008: Subscription-Usage Observability — Prometheus Gauge to Mimir + Span Attributes to Tempo

*Date:* 2026-06-18
*Status:* accepted

## Context

The ChatGPT/Codex backend reports each account's **subscription-usage state** —
the rolling quota windows that throttle a ChatGPT subscription — on the response
headers of the `/responses` and `/v1/chat/completions` calls. There are two
windows: a short **primary** (~5h) and a longer **secondary** (weekly). For
each, the backend reports a consumed percentage, the window length (minutes),
and an absolute reset instant, using the `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-at}`
header family — the same one the open-source OpenAI `codex` CLI parses
(`codex-rs/codex-api/src/rate_limits.rs`).

Until now codex-proxy **discarded** this data: `sanitized_headers()`
(`src/server/stream.rs`) forwards the quota headers downstream unchanged but
never reads them, and the proxy exposes no metrics (ADR 005 added OTLP **traces
only** — no `MeterProvider`, no `/metrics`). So the operational questions we
care about — *which account is about to hit its 5h cap? are we burning the
weekly allowance too fast? is one account carrying disproportionate load?* —
were unanswerable.

Deployment shape matters: each backend pod fronts exactly one account
(`garden/deploy/services/litellm/codex-proxy.ts`), so a pod's upstream responses
describe *that account's* quota. The router pod never sees its own quota (it only
relays backend responses), so account attribution is unambiguous **only at the
backend pod**.

This ADR is the codex-proxy-rs realization of **garden ADR 101**, which decided
the cross-repo design; it is recorded here because adding a metrics subsystem +
dependency + HTTP route is a local architectural decision (cf. ADR 005 for the
analogous trace-export subsystem).

## Decision

Read the quota headers off each upstream response in the backend data-plane
handlers (`chat`, `responses`) and emit them on **two complementary paths**:

1. **Monitoring system of record → Prometheus gauge → Mimir.** A new
   `prometheus`-backed registry (`src/metrics.rs`) exposes a `GET /metrics`
   endpoint with per-account, per-window gauges:
   - `codex_subscription_used_percent{account, window}` (0–100)
   - `codex_subscription_window_seconds{account, window}` (window-minutes × 60 —
     emitted in the Prometheus base unit, seconds)
   - `codex_subscription_reset_timestamp_seconds{account, window}` (absolute
     Unix timestamp)

   A gauge holds its last scraped value, which is the correct semantics for a
   *level* like a quota, and Mimir is where the platform's dashboards and alerts
   live. garden adds a named metrics port + Alloy scrape job (ADR 101).

2. **Per-request forensics → span attributes → Tempo.** The same values are
   attached to the existing `http.request` span via
   `OpenTelemetrySpanExt::set_attribute` (`codex.subscription.{primary,secondary}.{used_percent,window_seconds,reset_timestamp_seconds}`),
   answering "what was this account's quota *when this request ran*?" This reuses
   the ADR 005 trace exporter and is an inert no-op when no OTLP layer is
   installed.

Key invariants:

- **The `/metrics` path is independent of OTLP.** The gauges are always live, so
  a missing/misconfigured trace exporter can never silently disable Mimir
  scraping (the primary monitoring path). Only the span-attribute path is gated
  on OTLP (inherently — `set_attribute` is a no-op without the layer).
- **The `account` label is a stable, non-secret alias** from the new
  `CODEX_PROXY_ACCOUNT` env (`"unknown"` when unset), never a credential or
  client-supplied value. It cannot be sourced from `OTEL_RESOURCE_ATTRIBUTES`
  (`codex.account`) because garden sets that only when OTLP is configured, which
  would couple the metrics label to the trace path.
- **Only the named quota headers are read** — never the full `HeaderMap` — so
  authorization headers, cookies, and session identifiers never reach metrics,
  traces, or logs. Parsing is best-effort: absent/malformed values are dropped,
  never fail a request.
- **Reset is published as an absolute timestamp, not a countdown.** A gauge does
  not tick down between scrapes, so a "seconds remaining" series would read
  stale on an idle pod; dashboards derive remaining time as
  `codex_subscription_reset_timestamp_seconds - time()`.

## Alternatives Considered

1. **Span attributes → Tempo only (TraceQL metrics for dashboards)** – Reuses the
   existing exporter with no new dependency, but a quota *level* is poorly served
   by event-sampled trace metrics (no held last-value, weaker alerting), and
   garden's Tempo has the `metrics_generator`/`local-blocks` processor disabled.
   Kept as the forensics path, rejected as the monitoring system of record.
2. **Prometheus gauge → Mimir only** – The correct monitoring choice (path 1),
   but discards cheap, high-value request-level correlation the trace path gives
   essentially for free. Adopted *and* complemented rather than chosen alone.
3. **Derive metrics in LiteLLM from the forwarded headers** – codex-proxy already
   passes the quota headers through, so LiteLLM could observe them. Rejected:
   LiteLLM fronts the router and sees a blended cross-account stream, losing the
   per-account attribution only the backend pod has.
4. **`metrics` + `metrics-exporter-prometheus` facade** – Ergonomic, but a global
   recorder/facade is heavier and less explicit than this crate's style. The
   `prometheus` crate with an explicit `Registry` in `AppState` keeps the wiring
   testable and visible (matching ADR 004/005's explicitness).

## Consequences

- **Pros:**

  - Per-account subscription usage becomes a first-class, alertable signal in
    Mimir, with gauge semantics that match quota semantics.
  - Request-level forensics in Tempo at near-zero marginal cost (reuses ADR 005).
  - No Prometheus Operator and no Tempo `metrics_generator` required; the
    `/metrics` path is decoupled from OTLP.
  - Account attribution is correct by construction (emitted at the backend pod).

- **Cons:**

  - Adds a dependency (`prometheus`, default-features off) and an HTTP route.
  - Two emit paths to keep in sync if the header schema changes.
  - The gauge refreshes only when a request flows through that account's pod; an
    idle account shows its last-seen value (correct for a level, but worth noting
    on dashboards).
  - The exact `x-codex-*` header names track the (undocumented) backend contract;
    they are verified against the `openai/codex` CLI source but could drift.

## Technical Details

- `src/ratelimit.rs` — pure parsing of the `x-codex-*` headers into
  `RateLimits { primary, secondary }`; best-effort `Option` fields, finite-checked
  `f64`, `i64`. `used-percent` is the per-window anchor.
- `src/metrics.rs` — `Metrics` (registry + three `GaugeVec`s) in `AppState`;
  `observe_headers(account, &HeaderMap)` parses, sets the gauges, and attaches the
  span attributes; `render()` emits the text exposition format.
- `src/server/misc.rs` — open `GET /metrics` handler (scraped without the admin
  key, like `/health`; the series carry no secrets and exposure is bounded to the
  cluster by the Service).
- `src/server/{chat,responses}.rs` — call `state.metrics.observe_headers(...)`
  right after `send_with_retry`, capturing quota headers on success and 429s.
- `src/config.rs` / `src/main.rs` — `CODEX_PROXY_ACCOUNT` → `AppState.account`.

## Supersedes / Dependencies

- depends on: `005-otel-otlp-trace-export.md` (the span-attribute path reuses the
  OTLP exporter and the `http.request` span)
- realizes: garden ADR 101 (`docs/internal/decisions/101-codex-subscription-usage-metrics.md`)
