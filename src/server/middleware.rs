//! Request-logging and admin-auth middleware. Ports of Go
//! `loggingMiddleware` (`server.go:71-87`) and `adminMiddleware`
//! (`admin.go:12-71`).
//!
//! Like Go, the admin gate protects the data plane (`/v1/chat/completions`,
//! `/v1/responses`) as well as `/admin/*`: clients authenticate to the proxy
//! with `ADMIN_API_KEY` via `Authorization: Bearer <key>` or
//! `X-API-Key: <key>`. The key is snapshotted at startup (Go reads the env
//! var per request — equivalent in a k8s pod, documented in ADR 004).

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry_http::HeaderExtractor;
use tracing::Instrument as _;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use super::AppState;
use super::error::ApiError;

pub async fn log_requests(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let user_agent = request
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let start = std::time::Instant::now();

    // Continue LiteLLM's distributed trace: extract the inbound W3C context and
    // parent the per-request span on it (ADR 005 §5). With no OTLP layer
    // installed this is an inert no-op; with no inbound `traceparent` the span
    // is a fresh root.
    let parent_cx = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let span = tracing::info_span!(
        "http.request",
        otel.name = %format!("{method} {}", uri.path()),
        http.request.method = %method,
        url.path = %uri.path(),
        http.response.status_code = tracing::field::Empty,
    );
    // Errors only when no OTLP layer is installed (export disabled) — benign.
    let _ = span.set_parent(parent_cx);

    async move {
        let response = next.run(request).await;
        let status = response.status().as_u16();
        tracing::Span::current().record("http.response.status_code", status);

        tracing::info!(
            method = %method,
            uri = %uri,
            user_agent = %user_agent,
            status = status,
            duration_ms = start.elapsed().as_millis() as u64,
            "request",
        );
        response
    }
    .instrument(span)
    .await
}

pub async fn admin_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let Some(admin_key) = state.admin_api_key.as_deref().filter(|k| !k.is_empty()) else {
        tracing::error!("ADMIN_API_KEY environment variable not set");
        return Err(ApiError::AdminNotConfigured);
    };

    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    let x_api_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok());

    let provided = if let Some(auth) = auth_header.filter(|v| !v.is_empty()) {
        // Go: exactly two space-separated parts, case-insensitive "Bearer".
        let parts: Vec<&str> = auth.split(' ').collect();
        if parts.len() != 2 || !parts[0].eq_ignore_ascii_case("bearer") {
            tracing::warn!(uri = %request.uri(), "invalid Authorization header format for admin endpoint");
            return Err(ApiError::Unauthorized(
                "Invalid Authorization header format",
            ));
        }
        parts[1]
    } else if let Some(key) = x_api_key.filter(|v| !v.is_empty()) {
        key
    } else {
        tracing::warn!(uri = %request.uri(), "missing Authorization or X-API-Key header for admin endpoint");
        return Err(ApiError::Unauthorized("Unauthorized"));
    };

    // Constant-time comparison (hardening over Go's `!=`): the timing of an
    // auth-boundary compare shouldn't reveal how many leading bytes matched.
    let matches = provided.len() == admin_key.len()
        && provided
            .as_bytes()
            .iter()
            .zip(admin_key.as_bytes())
            .fold(0u8, |acc, (lhs, rhs)| acc | (lhs ^ rhs))
            == 0;
    if !matches {
        tracing::warn!(uri = %request.uri(), "invalid admin API key provided");
        return Err(ApiError::Unauthorized("Unauthorized"));
    }

    Ok(next.run(request).await)
}
