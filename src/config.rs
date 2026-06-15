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
fn otlp_provider(env: &str) -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

    // Gate: export is on when either standard OTLP endpoint var is set — the
    // general `OTEL_EXPORTER_OTLP_ENDPOINT` or the per-signal
    // `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (which the SDK honors at higher
    // precedence). Gating on only the general one would silently disable a
    // valid traces-only configuration. Read directly (not via clap Config)
    // because these vars are consumed by the OTel SDK, never by our own surface.
    let env_set = |var| std::env::var(var).is_ok_and(|v| !v.trim().is_empty());
    if !env_set("OTEL_EXPORTER_OTLP_ENDPOINT") && !env_set("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
        return None;
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
            // Misconfiguration must not take the server down — log and fall
            // back to logging-only. (Export is best-effort; ADR 005 Risks.)
            tracing::error!(error = %err, "failed to build OTLP span exporter; tracing export disabled");
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
    let service_name_in_attrs = std::env::var("OTEL_RESOURCE_ATTRIBUTES").is_ok_and(|attrs| {
        attrs
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .any(|(k, v)| k.trim() == "service.name" && !v.trim().is_empty())
    });
    if std::env::var_os("OTEL_SERVICE_NAME").is_none() && !service_name_in_attrs {
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
    use super::otlp_provider;

    /// The gate: OTLP export is built only when one of the standard OTLP
    /// endpoint env vars is set and non-empty — the general
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` or the per-signal
    /// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`. Unset/blank → `None` → logging-only,
    /// the pre-ADR-005 behavior the whole "feature-dark until configured" claim
    /// rests on.
    #[test]
    fn otlp_provider_is_gated_on_endpoint_env() {
        const GENERAL: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
        const TRACES: &str = "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT";

        // Save + restore ambient values so a developer with OTEL configured in
        // their shell doesn't get a false failure (and vice versa).
        let saved_general = std::env::var_os(GENERAL);
        let saved_traces = std::env::var_os(TRACES);
        // SAFETY: single-threaded access within this serial test; no other test
        // reads or writes these vars. `set_var`/`remove_var` are unsafe in 2024.
        let clear = || unsafe {
            std::env::remove_var(GENERAL);
            std::env::remove_var(TRACES);
        };
        let drain = |provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>| {
            // Drain the background batch processor deterministically.
            if let Some(provider) = provider {
                let _ = provider.shutdown();
            }
        };

        clear();
        assert!(otlp_provider("test").is_none(), "no endpoint → no exporter");

        unsafe { std::env::set_var(GENERAL, "   ") };
        assert!(
            otlp_provider("test").is_none(),
            "blank endpoint → no exporter"
        );

        unsafe { std::env::set_var(GENERAL, "http://127.0.0.1:4318") };
        let provider = otlp_provider("test");
        assert!(provider.is_some(), "general endpoint set → exporter built");
        drain(provider);

        // The per-signal var alone must also enable export (it would otherwise
        // be a silently-dropped valid config — the bug this gate guards).
        clear();
        unsafe { std::env::set_var(TRACES, "http://127.0.0.1:4318/v1/traces") };
        let provider = otlp_provider("test");
        assert!(
            provider.is_some(),
            "traces-specific endpoint set → exporter built"
        );
        drain(provider);

        // SAFETY: see above.
        unsafe {
            match saved_general {
                Some(value) => std::env::set_var(GENERAL, value),
                None => std::env::remove_var(GENERAL),
            }
            match saved_traces {
                Some(value) => std::env::set_var(TRACES, value),
                None => std::env::remove_var(TRACES),
            }
        }
    }

    /// The propagation seam codex-proxy depends on: a W3C `traceparent` must
    /// round-trip through the exact carrier types used in the middleware
    /// (`HeaderExtractor`) and upstream client (`HeaderInjector`) over an
    /// `http::HeaderMap`. This is the wiring most likely to break on an http /
    /// opentelemetry-http version bump, so pin it with a known vector.
    #[test]
    fn w3c_traceparent_round_trips_through_carriers() {
        use opentelemetry::propagation::TextMapPropagator;
        use opentelemetry::trace::TraceContextExt as _;
        use opentelemetry_http::{HeaderExtractor, HeaderInjector};
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

        // Inject — the upstream path (codex-proxy → ChatGPT). Same trace_id
        // continues, so Tempo nests the hop under the inbound trace.
        let mut outbound = http::HeaderMap::new();
        propagator.inject_context(&cx, &mut HeaderInjector(&mut outbound));
        let injected = outbound
            .get("traceparent")
            .expect("traceparent injected")
            .to_str()
            .unwrap();
        assert!(
            injected.contains(trace_id),
            "injected traceparent {injected} must carry the same trace_id",
        );
    }
}
