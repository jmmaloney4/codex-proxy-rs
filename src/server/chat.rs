//! POST /v1/chat/completions — port of Go `chatCompletionsHandler`
//! (`server.go:127-270`).

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use serde_json::Value;

use super::AppState;
use super::error::ApiError;
use super::stream::{RelayMode, mirror_error_response, relay_response, response_reader};
use crate::buffered::buffer_chat_completion;
use crate::request::build_codex_request_body;
use crate::upstream::send_with_retry;

pub async fn chat_completions(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let request: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Failed to parse request body".to_string()))?;

    // Go: only an explicit `"stream": true` selects streaming.
    let stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let target = build_codex_request_body(&request);
    // build_codex_request_body always sets the normalized model.
    let normalized_model = target
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(crate::model::GPT5)
        .to_string();
    let message_count = request
        .get("messages")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    tracing::info!(
        model = %normalized_model,
        stream,
        message_count,
        "chat completions request",
    );

    let out = Bytes::from(
        serde_json::to_vec(&target).map_err(|_| ApiError::Internal("Failed to process request"))?,
    );

    let resp = send_with_retry(&state.http, &state.creds, &state.upstream_url, out)
        .await
        .map_err(ApiError::Upstream)?;

    if stream {
        if resp.status() != reqwest::StatusCode::OK {
            return Ok(mirror_error_response(resp).await);
        }
        return Ok(relay_response(
            resp,
            RelayMode::Rewrite {
                model: normalized_model,
            },
            state.relay.clone(),
        ));
    }

    if resp.status() != reqwest::StatusCode::OK {
        return Ok(mirror_error_response(resp).await);
    }

    let completion = buffer_chat_completion(response_reader(resp), &normalized_model)
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "failed to buffer chat completion");
            ApiError::Internal("Failed to process streaming response")
        })?;
    Ok(Json(completion).into_response())
}
