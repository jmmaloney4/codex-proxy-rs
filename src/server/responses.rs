//! POST /v1/responses — port of Go `responsesHandler` (`server.go:272-382`).

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use serde_json::Value;

use super::AppState;
use super::error::ApiError;
use super::stream::{RelayMode, mirror_error_response, relay_response};
use crate::request::{
    resolve_reasoning_effort, resolve_request_model, transform_responses_request_body,
};
use crate::upstream::send_with_retry;

pub async fn responses(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let mut request: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Failed to parse request body".to_string()))?;

    let requested_model = resolve_request_model(&request);
    let requested_effort = resolve_reasoning_effort(&request);
    let (normalized_model, clamped_effort) =
        transform_responses_request_body(&mut request, &requested_model, &requested_effort);
    tracing::info!(
        model = %normalized_model,
        effort = %clamped_effort,
        "responses request",
    );

    let out = Bytes::from(
        serde_json::to_vec(&request)
            .map_err(|_| ApiError::Internal("Failed to process request"))?,
    );

    let resp = send_with_retry(&state.http, &state.creds, &state.upstream_url, out)
        .await
        .map_err(ApiError::Upstream)?;

    // Go: >= 400 is logged with a body preview and passed through.
    if resp.status().as_u16() >= 400 {
        return Ok(mirror_error_response(resp).await);
    }

    Ok(relay_response(
        resp,
        RelayMode::PassThrough,
        state.relay.clone(),
    ))
}
