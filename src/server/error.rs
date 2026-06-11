//! Handler error type. Status codes and message texts mirror Go's handlers;
//! bodies are JSON `{"error": "..."}` where Go used `http.Error` text/plain —
//! a documented divergence (ADR 004) friendlier to OpenAI SDK clients.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::upstream::UpstreamError;

#[derive(Debug)]
pub enum ApiError {
    /// 400 with the Go message text.
    BadRequest(String),
    /// 401 from the admin middleware.
    Unauthorized(&'static str),
    /// 500 "Admin API not configured" (ADMIN_API_KEY unset).
    AdminNotConfigured,
    /// 503 wrapping upstream/credential failures.
    Upstream(UpstreamError),
    /// 500 with a fixed message.
    Internal(&'static str),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.to_string()),
            ApiError::AdminNotConfigured => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Admin API not configured".to_string(),
            ),
            ApiError::Upstream(err) => {
                tracing::error!(error = %err, "upstream request failed");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Failed to communicate with upstream API: {err}"),
                )
            }
            ApiError::Internal(msg) => {
                tracing::error!(message = msg, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string())
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
