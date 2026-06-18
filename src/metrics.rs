//! Prometheus metrics for the `/metrics` endpoint (ADR 008; garden ADR 101).
//!
//! Subscription-usage gauges, labeled by a stable non-secret `account` alias and
//! the quota `window` (`primary`/`secondary`). The reset instant is published as
//! an **absolute Unix timestamp**, not a "seconds remaining" countdown: a
//! Prometheus gauge holds its last scraped value and does not tick down on its
//! own, so a countdown would read stale on an idle backend pod (ADR 101
//! Appendix B). Dashboards derive time-remaining as
//! `codex_subscription_reset_timestamp_seconds - time()`.
//!
//! This pull-based path is intentionally **independent of OTLP**: the gauges are
//! always live so a missing/misconfigured trace exporter can never silently
//! disable Mimir scraping (the primary monitoring path; ADR 101). The same
//! values are additionally attached to the current `http.request` span for
//! per-request forensics — that part is an inert no-op when no OTLP layer is
//! installed.

use axum::http::HeaderMap;
use prometheus::{GaugeVec, Opts, Registry, TextEncoder};
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::ratelimit::{RateLimits, Window};

const LABELS: [&str; 2] = ["account", "window"];

/// Owns the registry and the subscription gauges. Held in `AppState` and shared
/// across requests; cloning is cheap (the gauges are `Arc`-backed internally).
#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    used_percent: GaugeVec,
    window_seconds: GaugeVec,
    reset_timestamp_seconds: GaugeVec,
}

impl Metrics {
    pub fn new() -> Self {
        let used_percent = GaugeVec::new(
            Opts::new(
                "codex_subscription_used_percent",
                "Fraction (0-100) of the subscription quota window consumed.",
            ),
            &LABELS,
        )
        .expect("static metric opts");
        let window_seconds = GaugeVec::new(
            Opts::new(
                "codex_subscription_window_seconds",
                "Rolling subscription quota window duration, in seconds.",
            ),
            &LABELS,
        )
        .expect("static metric opts");
        let reset_timestamp_seconds = GaugeVec::new(
            Opts::new(
                "codex_subscription_reset_timestamp_seconds",
                "Absolute Unix timestamp (seconds) at which the quota window resets.",
            ),
            &LABELS,
        )
        .expect("static metric opts");

        let registry = Registry::new();
        registry
            .register(Box::new(used_percent.clone()))
            .expect("register used_percent");
        registry
            .register(Box::new(window_seconds.clone()))
            .expect("register window_seconds");
        registry
            .register(Box::new(reset_timestamp_seconds.clone()))
            .expect("register reset_timestamp_seconds");

        Self {
            registry,
            used_percent,
            window_seconds,
            reset_timestamp_seconds,
        }
    }

    /// Parse the quota headers off an upstream response and emit them: set the
    /// gauges (always) and attach span attributes (OTLP no-op when disabled).
    /// A response without quota headers leaves the gauges untouched — they keep
    /// their last value, which is the correct reading for an idle account.
    pub fn observe_headers(&self, account: &str, headers: &HeaderMap) {
        let limits = RateLimits::from_headers(headers);
        if limits.is_empty() {
            return;
        }
        if let Some(window) = limits.primary {
            self.observe_window(account, WindowKind::Primary, &window);
        }
        if let Some(window) = limits.secondary {
            self.observe_window(account, WindowKind::Secondary, &window);
        }
    }

    fn observe_window(&self, account: &str, kind: WindowKind, window: &Window) {
        let label = kind.label();
        self.used_percent
            .with_label_values(&[account, label])
            .set(window.used_percent);
        if let Some(minutes) = window.window_minutes {
            self.window_seconds
                .with_label_values(&[account, label])
                .set((minutes * 60) as f64);
        }
        if let Some(reset_at) = window.reset_at {
            self.reset_timestamp_seconds
                .with_label_values(&[account, label])
                .set(reset_at as f64);
        }

        // Per-request forensics on the current span (ADR 101): inert when no
        // OTLP layer is installed. The account already rides on the resource
        // (`codex.account`), so the span carries only the per-request values.
        let span = tracing::Span::current();
        span.set_attribute(kind.used_percent_attr(), window.used_percent);
        if let Some(minutes) = window.window_minutes {
            span.set_attribute(kind.window_seconds_attr(), minutes * 60);
        }
        if let Some(reset_at) = window.reset_at {
            span.set_attribute(kind.reset_attr(), reset_at);
        }
    }

    /// Render the registry in the Prometheus text exposition format.
    pub fn render(&self) -> String {
        TextEncoder::new()
            .encode_to_string(&self.registry.gather())
            .unwrap_or_else(|err| {
                tracing::error!(error = %err, "failed to encode prometheus metrics");
                String::new()
            })
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Quota window discriminant — carries the static metric label and the static
/// span-attribute keys so neither allocates per request.
#[derive(Clone, Copy)]
enum WindowKind {
    Primary,
    Secondary,
}

impl WindowKind {
    fn label(self) -> &'static str {
        match self {
            WindowKind::Primary => "primary",
            WindowKind::Secondary => "secondary",
        }
    }

    fn used_percent_attr(self) -> &'static str {
        match self {
            WindowKind::Primary => "codex.subscription.primary.used_percent",
            WindowKind::Secondary => "codex.subscription.secondary.used_percent",
        }
    }

    fn window_seconds_attr(self) -> &'static str {
        match self {
            WindowKind::Primary => "codex.subscription.primary.window_seconds",
            WindowKind::Secondary => "codex.subscription.secondary.window_seconds",
        }
    }

    fn reset_attr(self) -> &'static str {
        match self {
            WindowKind::Primary => "codex.subscription.primary.reset_timestamp_seconds",
            WindowKind::Secondary => "codex.subscription.secondary.reset_timestamp_seconds",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn renders_labeled_gauges_in_seconds() {
        let metrics = Metrics::new();
        metrics.observe_headers(
            "main",
            &headers(&[
                ("x-codex-primary-used-percent", "12.5"),
                ("x-codex-primary-window-minutes", "300"),
                ("x-codex-primary-reset-at", "1704069000"),
            ]),
        );
        let out = metrics.render();
        assert!(
            out.contains(
                "codex_subscription_used_percent{account=\"main\",window=\"primary\"} 12.5"
            ),
            "used_percent series missing:\n{out}"
        );
        // 300 minutes → 18000 seconds (base-unit conversion).
        assert!(
            out.contains(
                "codex_subscription_window_seconds{account=\"main\",window=\"primary\"} 18000"
            ),
            "window_seconds not converted to seconds:\n{out}"
        );
        assert!(
            out.contains("codex_subscription_reset_timestamp_seconds{account=\"main\",window=\"primary\"} 1704069000"),
            "reset timestamp series missing:\n{out}"
        );
    }

    #[test]
    fn no_quota_headers_emits_no_series() {
        let metrics = Metrics::new();
        metrics.observe_headers("main", &HeaderMap::new());
        assert!(
            !metrics.render().contains("codex_subscription_"),
            "idle response must not create series"
        );
    }

    #[test]
    fn partial_window_emits_only_present_fields() {
        let metrics = Metrics::new();
        metrics.observe_headers(
            "main",
            &headers(&[("x-codex-secondary-used-percent", "73")]),
        );
        let out = metrics.render();
        assert!(
            out.contains(
                "codex_subscription_used_percent{account=\"main\",window=\"secondary\"} 73"
            )
        );
        assert!(
            !out.contains("codex_subscription_window_seconds"),
            "absent window-minutes must not emit a series:\n{out}"
        );
        assert!(
            !out.contains("codex_subscription_reset_timestamp_seconds"),
            "absent reset-at must not emit a series:\n{out}"
        );
    }
}
