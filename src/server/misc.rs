//! Open endpoints: /health, /v1/models, and the 404 fallback. Ports of Go
//! `healthHandler`, `modelsHandler`, `notFoundHandler`.
//!
//! The models payload is a verbatim dump of Go `supportedModels()` (see the
//! dump procedure in the PR description), with the not-planned
//! `gpt-5.3-codex-spark` entries filtered out (ADR 004).

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use serde_json::json;

use super::AppState;

pub static MODELS_JSON: &str = include_str!("models.json");

pub async fn models() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], MODELS_JSON)
}

pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Prometheus text exposition of the subscription-usage gauges (ADR 008). The
/// 0.0.4 version token is the format the Prometheus/Alloy scrapers expect.
pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        state.metrics.render(),
    )
}

pub async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" })))
}
