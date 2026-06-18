//! Parse the ChatGPT/Codex subscription rate-limit headers off an upstream
//! response (garden ADR 101, codex-proxy-rs ADR 008).
//!
//! The backend reports each account's rolling quota windows on the response
//! headers of the `/responses` (and `/v1/chat/completions`) calls — a short
//! **primary** window and a longer **secondary** window — using the same
//! `x-codex-{primary,secondary}-{used-percent,window-minutes,reset-at}` family
//! the open-source OpenAI `codex` CLI parses (`codex-rs/codex-api`). We read
//! **only** these named headers — never the full `HeaderMap` — so authorization
//! headers, cookies, and session identifiers never reach logs, metrics, or
//! traces (ADR 101 Appendix A).
//!
//! All fields are best-effort `Option`s: the headers are not guaranteed present
//! (older backends, error responses), and a malformed value is dropped rather
//! than failing the request — this is observability, never a hard dependency.

use axum::http::HeaderMap;

/// One rolling quota window (primary or secondary).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// Fraction of the window consumed, clamped to 0–100 (the documented
    /// domain) so a bad upstream value can't poison gauges/alerts with an
    /// impossible usage. Clamped, not dropped: the percentage is the anchor of
    /// the whole window, and an over-cap reading still belongs at 100 rather
    /// than erasing all visibility for that account/window.
    pub used_percent: f64,
    /// Rolling window duration, in minutes (as reported by the backend).
    pub window_minutes: Option<i64>,
    /// Absolute Unix timestamp (seconds) at which the window resets. The
    /// backend already reports an absolute instant (`-reset-at`), not a
    /// countdown — see ADR 101 Appendix B for why that matters.
    pub reset_at: Option<i64>,
}

/// The primary + secondary windows parsed from an upstream response.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RateLimits {
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
}

impl RateLimits {
    /// Parse the `x-codex-*` quota headers. Returns all-`None` when none are
    /// present (the common case for non-Codex or error responses).
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            primary: Window::from_headers(headers, "primary"),
            secondary: Window::from_headers(headers, "secondary"),
        }
    }

    /// Whether either window carried any data worth emitting.
    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.secondary.is_none()
    }
}

impl Window {
    /// Parse one window (`kind` is `"primary"` or `"secondary"`). A window is
    /// only materialized when its `used-percent` header is present and a finite
    /// number — that header is the anchor; `window-minutes`/`reset-at` are
    /// optional refinements.
    fn from_headers(headers: &HeaderMap, kind: &str) -> Option<Self> {
        // Clamp to the documented 0–100 domain (see the field doc): an
        // out-of-range value is a backend bug, but clamping keeps the window
        // visible (and alerting at 100) rather than dropping it entirely.
        let used_percent =
            parse_f64(headers, &format!("x-codex-{kind}-used-percent"))?.clamp(0.0, 100.0);
        Some(Self {
            used_percent,
            window_minutes: parse_i64(headers, &format!("x-codex-{kind}-window-minutes")),
            reset_at: parse_i64(headers, &format!("x-codex-{kind}-reset-at")),
        })
    }
}

/// Read a header as a finite `f64`; `None` on absent / non-ASCII / unparseable /
/// non-finite (NaN/∞ would poison gauges and dashboards).
fn parse_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    let value = headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    value.is_finite().then_some(value)
}

/// Read a header as an `i64`; `None` on absent / non-ASCII / unparseable.
fn parse_i64(headers: &HeaderMap, name: &str) -> Option<i64> {
    headers.get(name)?.to_str().ok()?.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

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
    fn parses_both_windows() {
        let limits = RateLimits::from_headers(&headers(&[
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1704069000"),
            ("x-codex-secondary-used-percent", "73"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", "1704669000"),
        ]));
        assert_eq!(
            limits.primary,
            Some(Window {
                used_percent: 12.5,
                window_minutes: Some(300),
                reset_at: Some(1704069000),
            })
        );
        assert_eq!(
            limits.secondary,
            Some(Window {
                used_percent: 73.0,
                window_minutes: Some(10080),
                reset_at: Some(1704669000),
            })
        );
        assert!(!limits.is_empty());
    }

    #[test]
    fn absent_headers_yield_empty() {
        let limits = RateLimits::from_headers(&HeaderMap::new());
        assert!(limits.is_empty());
        assert_eq!(limits, RateLimits::default());
    }

    #[test]
    fn used_percent_is_the_anchor() {
        // window/reset present but no used-percent → window not materialized.
        let limits = RateLimits::from_headers(&headers(&[
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1704069000"),
        ]));
        assert!(limits.primary.is_none());
    }

    #[test]
    fn used_percent_alone_is_enough() {
        let limits = RateLimits::from_headers(&headers(&[("x-codex-primary-used-percent", "5.0")]));
        assert_eq!(
            limits.primary,
            Some(Window {
                used_percent: 5.0,
                window_minutes: None,
                reset_at: None,
            })
        );
    }

    #[test]
    fn non_finite_and_garbage_are_dropped() {
        for bad in ["NaN", "inf", "-inf", "", "abc", "  "] {
            let limits =
                RateLimits::from_headers(&headers(&[("x-codex-primary-used-percent", bad)]));
            assert!(limits.primary.is_none(), "{bad:?} should not parse");
        }
        // A garbage window-minutes does not sink the window; used-percent holds.
        let limits = RateLimits::from_headers(&headers(&[
            ("x-codex-primary-used-percent", "10"),
            ("x-codex-primary-window-minutes", "soon"),
        ]));
        assert_eq!(limits.primary.map(|w| w.window_minutes), Some(None));
    }

    #[test]
    fn used_percent_is_clamped_to_domain() {
        // Out-of-range values are clamped, not dropped — the window stays
        // visible and an over-cap reading still alerts at 100.
        for (raw, expected) in [("150", 100.0), ("-5", 0.0), ("100", 100.0), ("0", 0.0)] {
            let limits =
                RateLimits::from_headers(&headers(&[("x-codex-primary-used-percent", raw)]));
            assert_eq!(
                limits.primary.map(|w| w.used_percent),
                Some(expected),
                "{raw} should clamp to {expected}"
            );
        }
    }
}
