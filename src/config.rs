//! CLI + environment configuration (clap; figment is pinned-but-deferred per
//! issue #3) and tracing setup matching Go `internal/logger`.
//!
//! Legacy env names (`PORT`, `ADMIN_API_KEY`, `ANTHROPIC_API_KEY`,
//! `CLAUDE_USER_ID`, `ENV`) are honored for drop-in parity with the Go
//! deployment; new variables are `CODEX_PROXY_`-prefixed.

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CredsStore {
    /// Static token from ANTHROPIC_API_KEY / CLAUDE_USER_ID.
    Env,
    /// auth.json on a writable volume, with in-process OAuth refresh.
    Fs,
}

/// What this process does (ADR 007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProxyMode {
    /// Single-account backend: transform requests and call the ChatGPT/Codex
    /// backend directly. The default; unchanged behavior.
    Backend,
    /// Front the single-account backend pods, pin each conversation to one
    /// account, and reverse-proxy. Uses `CODEX_PROXY_ACCOUNTS` + optional Redis.
    Router,
}

#[derive(Debug, Parser)]
#[command(name = "codex-proxy", version, about)]
pub struct Config {
    /// Listen port.
    #[arg(long, env = "PORT", default_value_t = 9879)]
    pub port: u16,

    /// API key required on the data-plane and /admin routes.
    #[arg(long, env = "ADMIN_API_KEY", hide_env_values = true)]
    pub admin_api_key: Option<String>,

    /// Log mode: "development"/"dev"/"" → pretty console, else JSON.
    #[arg(long, env = "ENV", default_value = "development")]
    pub env: String,

    /// Process mode: single-account backend (default) or affinity router.
    #[arg(long = "mode", env = "CODEX_PROXY_MODE", value_enum, default_value_t = ProxyMode::Backend)]
    pub mode: ProxyMode,

    /// Router mode: comma-separated `slug=url` backend accounts, e.g.
    /// `main=http://codex-proxy-main.codex-proxy.svc.cluster.local:9879,codex2=...`.
    #[arg(
        long = "codex-accounts",
        env = "CODEX_PROXY_ACCOUNTS",
        default_value = ""
    )]
    pub codex_accounts: String,

    /// Router mode: Redis URL for the conversation→account affinity store.
    /// Unset → the router runs without affinity (stateless round-robin).
    #[arg(
        long = "redis-url",
        env = "CODEX_PROXY_REDIS_URL",
        hide_env_values = true
    )]
    pub redis_url: Option<String>,

    /// Router mode: affinity pin TTL in seconds (default 1 day). Must be ≥ 1 —
    /// Redis `SET … EX 0` is a protocol error that would silently drop pins.
    #[arg(
        long = "affinity-ttl-secs",
        env = "CODEX_PROXY_AFFINITY_TTL_SECS",
        value_parser = clap::value_parser!(u64).range(1..),
        default_value_t = 86_400
    )]
    pub affinity_ttl_secs: u64,

    /// Credential store mode.
    #[arg(long = "creds-store", env = "CODEX_PROXY_CREDS_STORE", value_enum, default_value_t = CredsStore::Env)]
    pub creds_store: CredsStore,

    /// Path to auth.json for the fs credential store.
    /// Defaults to $XDG_CONFIG_HOME/codex-proxy/auth.json.
    #[arg(long = "creds-path", env = "CODEX_PROXY_CREDS_PATH")]
    pub creds_path: Option<std::path::PathBuf>,

    /// SSE keepalive interval in seconds. Zero is rejected: the periodic
    /// ping is also how the relay notices client disconnects while the
    /// upstream is stalled (the zero-disables semantic exists only at the
    /// relay-library level, for paused-clock tests).
    #[arg(
        long,
        env = "CODEX_PROXY_KEEPALIVE_SECS",
        value_parser = clap::value_parser!(u64).range(1..),
        default_value_t = 15
    )]
    pub keepalive_secs: u64,

    /// Static bearer token for the env credential store (legacy name).
    #[arg(
        long,
        env = "ANTHROPIC_API_KEY",
        hide_env_values = true,
        default_value = ""
    )]
    pub anthropic_api_key: String,

    /// Account ID for the env credential store (legacy name).
    #[arg(long, env = "CLAUDE_USER_ID", default_value = "")]
    pub claude_user_id: String,
}

/// Initialize logging and (optionally) OpenTelemetry OTLP trace export (ADR 005).
///
/// Logging is unchanged from Go-logger parity: ENV of ""/"dev"/"development" →
/// pretty console output, anything else → JSON to stderr.
///
/// OTLP trace export is **gated on an OTLP endpoint env var** (general
/// `OTEL_EXPORTER_OTLP_ENDPOINT` or per-signal
/// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`): when one is set, spans are batched and
/// exported over OTLP/HTTP (`http/protobuf`) to the garden `alloy-llm-traces`
/// collector, and the returned [`SdkTracerProvider`] must be flushed on shutdown
/// (see `main`). When neither is set, this returns `None` and behavior is
/// identical to before — fmt logging only, no OTel cost.
pub fn init_tracing(env: &str) -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, Layer};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let dev = matches!(env, "" | "dev" | "development");

    // fmt layer: pretty (dev) or JSON, always to stderr — boxed so both arms
    // share one type.
    let fmt_layer = if dev {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stderr)
            .boxed()
    };

    // OTLP export is opt-in: only wire the OpenTelemetry layer when an endpoint
    // is configured. `Option<Layer>` is itself a `Layer`, so the `None` arm is
    // a true no-op with zero OTel machinery installed.
    let (otel_layer, provider) = match otlp_provider(env) {
        Some(provider) => {
            use opentelemetry::trace::TracerProvider as _;
            let tracer = provider.tracer("codex-proxy");
            let layer = tracing_opentelemetry::layer().with_tracer(tracer);
            (Some(layer), Some(provider))
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    provider
}

/// Build the OTLP/HTTP tracer provider when an OTLP endpoint env var is set;
/// otherwise `None`. The endpoint, protocol, and resource attributes are all
/// read from the standard `OTEL_*` env vars (ADR 005 §10) — garden injects them
/// the same way it does for LiteLLM.
/// OTLP export is enabled when either endpoint var holds a non-blank value.
/// Pure so the gate is tested without mutating process-global env (which races
/// other threads' `getenv` under the parallel test runner — UB in Rust 2024).
fn export_enabled(general_endpoint: Option<&str>, traces_endpoint: Option<&str>) -> bool {
    [general_endpoint, traces_endpoint]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty())
}

/// Whether `OTEL_RESOURCE_ATTRIBUTES` already declares a non-blank
/// `service.name` (a heuristic over the common `k=v,k=v` form — good enough to
/// decide whether to apply our fallback default, never to parse the full spec).
fn attrs_declare_service_name(attrs: Option<&str>) -> bool {
    attrs.is_some_and(|attrs| {
        attrs
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .any(|(key, value)| key.trim() == "service.name" && !value.trim().is_empty())
    })
}

fn otlp_provider(env: &str) -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

    // NOTE: this runs *before* `init_tracing` installs the subscriber, so
    // `tracing::*` events here would go to a no-op dispatcher and vanish.
    // Startup misconfiguration diagnostics therefore use `eprintln!` to stderr —
    // the same sink the fmt layer writes to — so operators actually see them.

    // Gate: export is on when either standard OTLP endpoint var is set — the
    // general `OTEL_EXPORTER_OTLP_ENDPOINT` or the per-signal
    // `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (which the SDK honors at higher
    // precedence). Gating on only the general one would silently disable a
    // valid traces-only configuration. Read directly (not via clap Config)
    // because these vars are consumed by the OTel SDK, never by our own surface.
    let general_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
    let traces_endpoint = std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok();
    if !export_enabled(general_endpoint.as_deref(), traces_endpoint.as_deref()) {
        return None;
    }

    // This build is OTLP/HTTP only (no `grpc-tonic` feature; ADR 005 §10) and
    // `.with_http()` forces the transport, so `OTEL_EXPORTER_OTLP_PROTOCOL=grpc`
    // is silently ineffective. Warn rather than let it surprise an operator who
    // also pointed at a gRPC-only port and then sees every export fail.
    let requested_protocol = std::env::var("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
        .or_else(|_| std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL"))
        .unwrap_or_default();
    if requested_protocol.contains("grpc") {
        eprintln!(
            "codex-proxy: WARN OTLP protocol gRPC requested ({requested_protocol}) but this build is HTTP-only; using http/protobuf"
        );
    }

    // Deliberately NO `.with_endpoint(endpoint)`: we let the SDK resolve the
    // target from `OTEL_EXPORTER_OTLP_ENDPOINT`, which appends the `/v1/traces`
    // signal path. Passing the raw value to `.with_endpoint()` would be treated
    // as the full traces URL and skip that suffix, POSTing to the collector root
    // (→ 404, dropped spans). See opentelemetry-otlp `resolve_http_endpoint`.
    let exporter = match SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
    {
        Ok(exporter) => exporter,
        Err(err) => {
            // Misconfiguration must not take the server down — warn and fall
            // back to logging-only. (Export is best-effort; ADR 005 Risks.)
            // `eprintln!` per the pre-subscriber note above.
            eprintln!(
                "codex-proxy: ERROR failed to build OTLP span exporter; tracing export disabled: {err}"
            );
            return None;
        }
    };

    // `Resource::builder()` (not `builder_empty()`) ships the EnvResourceDetector
    // that merges OTEL_SERVICE_NAME / OTEL_RESOURCE_ATTRIBUTES (ADR 005 §4).
    let mut resource = Resource::builder()
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .with_attribute(KeyValue::new("deployment.environment", env.to_owned()));
    // Default service.name to `codex-proxy` only when neither standard mechanism
    // already supplies one — `OTEL_SERVICE_NAME`, or a `service.name=` entry in
    // `OTEL_RESOURCE_ATTRIBUTES`. The explicit `.with_service_name()` overrides
    // the EnvResourceDetector, so applying it unconditionally would clobber an
    // operator-provided identity and mislabel spans.
    // Treat a blank `OTEL_SERVICE_NAME` as unset (env templating often produces
    // empty strings) so the fallback still applies rather than exporting an
    // empty identity — consistent with the endpoint gate above.
    let service_name_set = std::env::var("OTEL_SERVICE_NAME").is_ok_and(|v| !v.trim().is_empty())
        || attrs_declare_service_name(std::env::var("OTEL_RESOURCE_ATTRIBUTES").ok().as_deref());
    if !service_name_set {
        resource = resource.with_service_name("codex-proxy");
    }

    // W3C trace context is the correlation spine (garden ADR 084): extract it
    // from inbound requests, inject it into upstream ones.
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    // ParentBased(AlwaysOn) (ADR 005 §9): follow LiteLLM's sampling decision
    // when a parent is propagated, sample everything when we are the root.
    let provider = SdkTracerProvider::builder()
        .with_resource(resource.build())
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::AlwaysOn)))
        .with_batch_exporter(exporter)
        .build();

    Some(provider)
}

#[cfg(test)]
mod tests {
    use super::{attrs_declare_service_name, export_enabled};

    /// The gate behind "feature-dark until configured": export turns on only
    /// when one of the standard OTLP endpoint vars holds a non-blank value —
    /// the general `OTEL_EXPORTER_OTLP_ENDPOINT` or the per-signal
    /// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`. Tested via the pure decision so no
    /// process-global env is mutated (that would race other threads under the
    /// parallel runner).
    #[test]
    fn export_gate_requires_a_non_blank_endpoint() {
        assert!(!export_enabled(None, None), "neither set → off");
        assert!(!export_enabled(Some("   "), Some("")), "blank values → off");
        assert!(
            export_enabled(Some("http://collector:4318"), None),
            "general endpoint → on",
        );
        assert!(
            export_enabled(None, Some("http://collector:4318/v1/traces")),
            "per-signal endpoint → on (would otherwise be a silently-dropped config)",
        );
    }

    /// The fallback `service.name` default must yield to a `service.name=`
    /// supplied via `OTEL_RESOURCE_ATTRIBUTES`, so it isn't clobbered.
    #[test]
    fn service_name_in_resource_attributes_is_detected() {
        assert!(!attrs_declare_service_name(None));
        assert!(!attrs_declare_service_name(Some(
            "deployment.environment=prod,codex.account=main"
        )));
        assert!(
            !attrs_declare_service_name(Some("service.name=")),
            "blank value does not count",
        );
        assert!(attrs_declare_service_name(Some("service.name=my-svc")));
        assert!(attrs_declare_service_name(Some(
            "deployment.environment=prod,service.name=my-svc,codex.account=main"
        )));
    }

    /// The inbound propagation seam codex-proxy depends on: a W3C `traceparent`
    /// must extract through the exact carrier type used in the middleware
    /// (`HeaderExtractor` over an `http::HeaderMap`) into a remote parent with
    /// the right trace_id. This is the wiring most likely to break on an http /
    /// opentelemetry-http version bump, so pin it with a known vector. (We do
    /// not inject outbound — the only upstream is external; see `upstream.rs`.)
    #[test]
    fn w3c_traceparent_extracts_into_remote_parent() {
        use opentelemetry::propagation::TextMapPropagator;
        use opentelemetry::trace::TraceContextExt as _;
        use opentelemetry_http::HeaderExtractor;
        use opentelemetry_sdk::propagation::TraceContextPropagator;

        let propagator = TraceContextPropagator::new();
        let trace_id = "4bf92f3577b34da6a3ce929d0e0e4736";
        let traceparent = format!("00-{trace_id}-00f067aa0ba902b7-01");

        // Extract — the middleware path (inbound LiteLLM → codex-proxy).
        let mut inbound = http::HeaderMap::new();
        inbound.insert("traceparent", traceparent.parse().unwrap());
        let cx = propagator.extract(&HeaderExtractor(&inbound));
        let extracted = cx.span().span_context().clone();
        assert_eq!(
            extracted.trace_id().to_string(),
            trace_id,
            "extracted trace_id must match the inbound traceparent",
        );
        assert!(extracted.is_remote(), "parent must be flagged remote");
    }
}
