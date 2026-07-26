//! POST /v1/responses — port of Go `responsesHandler` (`server.go:272-382`).

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use serde_json::Value;

use super::AppState;
use super::error::ApiError;
use super::stream::{
    RelayMode, is_event_stream, mirror_error_response, mirror_success_response, relay_response,
    response_reader,
};
use crate::buffered::buffer_responses_response;
use crate::request::{
    resolve_reasoning_effort, resolve_request_model, transform_responses_request_body,
};
use crate::upstream::send_with_retry;

pub async fn responses(State(state): State<AppState>, body: Bytes) -> Result<Response, ApiError> {
    let mut request: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Failed to parse request body".to_string()))?;

    // Read the caller's intent *before* the transform, which unconditionally
    // sets `stream: true` upstream (the Codex backend accepts nothing else).
    // Like `/v1/chat/completions`, only an explicit `"stream": true` selects a
    // streamed downstream response; anything else gets the aggregated object.
    let client_wants_stream = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let requested_model = resolve_request_model(&request);
    let requested_effort = resolve_reasoning_effort(&request);
    let (normalized_model, clamped_effort) =
        transform_responses_request_body(&mut request, &requested_model, &requested_effort);
    tracing::info!(
        model = %normalized_model,
        effort = %clamped_effort,
        stream = client_wants_stream,
        "responses request",
    );

    let out = Bytes::from(
        serde_json::to_vec(&request)
            .map_err(|_| ApiError::Internal("Failed to process request"))?,
    );

    let resp = send_with_retry(
        &state.http,
        &state.creds,
        &state.upstream_url,
        out,
        &state.codex_cli_version,
    )
    .await
    .map_err(ApiError::Upstream)?;

    // Subscription-usage observability (ADR 008): read the quota headers off the
    // upstream response (success or 429) before relaying. Best-effort and
    // header-name-scoped — never touches the body or the full header set. This
    // handler is mounted only in Backend mode (see the `server::router` route
    // table; Router mode serves `/v1/responses` via `router::proxy`, which never
    // emits), so the per-account `account` label is always correct — the router
    // never mislabels its blended cross-account stream.
    state
        .metrics
        .observe_headers(&state.account, resp.headers());

    // Go: >= 400 is logged with a body preview and passed through.
    if resp.status().as_u16() >= 400 {
        return Ok(mirror_error_response(resp).await);
    }

    // Only SSE responses go through the relay (Go gates its SSE headers on the
    // same media-type check). A non-SSE success is mirrored verbatim: the
    // upstream ignored our forced `stream: true` and already answered with the
    // final object, which is exactly what either caller wants.
    if !is_event_stream(&resp) {
        return Ok(mirror_success_response(resp).await);
    }

    if client_wants_stream {
        return Ok(relay_response(
            resp,
            RelayMode::PassThrough,
            state.relay.clone(),
        ));
    }

    // The caller wanted a plain response but we had to ask upstream for a
    // stream, so collapse the SSE back into the single response object the
    // Responses API defines for a non-streaming call.
    let response = buffer_responses_response(response_reader(resp))
        .await
        .map_err(|err| {
            tracing::error!(error = %err, "failed to buffer responses stream");
            ApiError::Internal("Failed to process streaming response")
        })?;
    Ok(Json(response).into_response())
}
